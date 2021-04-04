use crate::{canvas::CanvasMessage, recording::MOVIE_TEXTURE_FORMAT, uniforms::UserUniform};
use crate::{
    recording::Recorder,
    utils::{AsyncTiffWriter, WriteFinished},
};
use crate::{
    uniforms,
    vector::{IntVector2, UIntVector2, Vector2},
};
use core::panic;
use imgui::{im_str, ImString, StyleColor};
use imgui::{Condition, FontSource};
use imgui_wgpu::RendererConfig;
use imgui_winit_support;
use log::{info, warn};
use std::{
    sync::mpsc::{Receiver, SyncSender},
    time::Instant,
    usize,
};
use wgpu::{PowerPreference, RequestAdapterOptions};
use winit::{event::*, window::Window};

/// Struct containing information the GUI is displaying and interacting with.
pub struct DashboardState {
    pub last_render_time: f64,
    pub frame_num: usize,
    pub frame_timeout_count: usize,
    pub mouse_pos: Vector2,
    pub render_window_size: IntVector2,
    pub paused: bool,
    pub show_titlebar: bool,
    pub painting_resolution: IntVector2,
    pub recording_resolution: IntVector2,
    pub painting_filename: String,
    pub recording_filename: String,
    /// Unit: seconds
    pub movie_framerate: i32,
    /// Only available on macOS.
    pub open_painting_externally: bool,
    pub pause_while_painting: bool,
    pub painting_progress_receiver: Option<Receiver<WriteFinished>>,
    pub shader_compilation_error_msg: Option<String>,
    pub painting_start_time: Option<std::time::Instant>,
    pub gui_uniforms: Vec<Box<dyn UserUniform>>,
}

impl DashboardState {
    pub fn new() -> DashboardState {
        DashboardState {
            last_render_time: 0.0,
            frame_num: 0,
            frame_timeout_count: 0,
            mouse_pos: Vector2::zero(),
            render_window_size: IntVector2::zero(),
            paused: false,
            show_titlebar: true,
            painting_resolution: IntVector2::zero(),
            recording_resolution: IntVector2::new(512, 512),
            painting_filename: String::from("Painting"),
            recording_filename: String::from("Muybridge"),
            movie_framerate: 60,
            open_painting_externally: true,
            pause_while_painting: true,
            painting_progress_receiver: None,
            shader_compilation_error_msg: None,
            painting_start_time: None,
            gui_uniforms: Vec::new(),
        }
    }
}

/// Message Enums used by [Dashboard] to send messages to interested parties.
pub enum DashboardMessage {
    PausePlayChanged,
    Play,
    Pause,
    TitlebarStatusChanged,
    PaintingRenderRequested(UIntVector2),
    PaintingResolutionUpdated(UIntVector2),
    MovieRenderRequested(UIntVector2),
    UniformUpdatedViaGUI(Box<dyn UserUniform>),
}

/// Centralized controller and GUI class.
/// Renders to its own window and provides controls for render [crate::canvas::Canvas]
/// Provides runtime stats and other useful information.
pub struct Dashboard {
    pub window: winit::window::Window,
    pub instance: wgpu::Instance,
    pub surface: wgpu::Surface,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    sc_desc: wgpu::SwapChainDescriptor,
    swap_chain: wgpu::SwapChain,

    clear_color: wgpu::Color,
    size: winit::dpi::PhysicalSize<u32>,
    imgui_context: imgui::Context,
    imgui_platform: imgui_winit_support::WinitPlatform,
    imgui_renderer: imgui_wgpu::Renderer,
    last_frame: std::time::Instant,
    hidpi_factor: f32,

    state: DashboardState,

    transmitter: SyncSender<DashboardMessage>,
    receiver: Receiver<CanvasMessage>,
    recorder: Option<Recorder>,
    last_movie_frame_time: Option<Instant>,
}

impl Dashboard {
    /// Construct a new [Dashboard].
    /// * `window` - The [winit::window::Window] this object will render to. Takes ownership.
    /// * `transmitter` - [std::sync::mpsc::Sender] object used to send [DashboardMessage]s to intererested parties.
    /// * `receiver` - [std::sync::mpsc::Receiver] object used to receive messages from [crate::canvas::Canvas]
    pub async fn new(
        window: Window,
        transmitter: SyncSender<DashboardMessage>,
        receiver: Receiver<CanvasMessage>,
    ) -> Self {
        let instance = wgpu::Instance::new(wgpu::BackendBit::PRIMARY);
        let size = window.inner_size();

        let surface: wgpu::Surface;
        unsafe {
            surface = instance.create_surface(&window);
        }

        let adapter = instance
            .request_adapter(&RequestAdapterOptions {
                compatible_surface: Some(&surface),
                power_preference: PowerPreference::LowPower,
            })
            .await
            .unwrap();
        let device_desc = wgpu::DeviceDescriptor {
            label: None,
            features: adapter.features(),
            limits: Default::default(),
        };

        let (device, mut queue) = adapter.request_device(&device_desc, None).await.unwrap();

        //------------------------------------------------------------------------------------------
        // Setup swap chain
        let sc_desc = wgpu::SwapChainDescriptor {
            usage: wgpu::TextureUsage::RENDER_ATTACHMENT,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            width: size.width,
            height: size.height,
            present_mode: wgpu::PresentMode::Fifo,
        };
        let swap_chain = device.create_swap_chain(&surface, &sc_desc);

        //------------------------------------------------------------------------------------------
        // ImGUI setup
        let hidpi_factor = window.scale_factor() as f32;
        let mut imgui = imgui::Context::create();
        let mut platform = imgui_winit_support::WinitPlatform::init(&mut imgui);
        platform.attach_window(
            imgui.io_mut(),
            &window,
            imgui_winit_support::HiDpiMode::Default,
        );
        let font_size = (18.0 * hidpi_factor) as f32;
        imgui.io_mut().font_global_scale = (1.0 / hidpi_factor) as f32;
        imgui.set_ini_filename(None);
        imgui.fonts().add_font(&[FontSource::TtfData {
            size_pixels: font_size,
            data: include_bytes!("../assets/Quicksand/static/Quicksand-Medium.ttf"),
            config: Some(imgui::FontConfig {
                oversample_v: hidpi_factor as i32,
                oversample_h: hidpi_factor as i32,
                size_pixels: font_size,
                ..Default::default()
            }),
        }]);

        //------------------------------------------------------------------------------------------
        // Setup ImGUI WGPU Renderer
        let clear_color = wgpu::Color {
            r: 0.1,
            g: 0.2,
            b: 0.3,
            a: 1.0,
        };
        let mut renderer_config = RendererConfig::new_srgb();
        renderer_config.texture_format = sc_desc.format;
        let renderer = imgui_wgpu::Renderer::new(&mut imgui, &device, &mut queue, renderer_config);
        let mut state = DashboardState::new();
        state.render_window_size = IntVector2::new(size.width as i32, size.height as i32);

        Self {
            window,
            instance,
            surface,
            adapter,
            device,
            queue,
            sc_desc,
            swap_chain,
            clear_color,
            size,
            imgui_context: imgui,
            imgui_platform: platform,
            imgui_renderer: renderer,
            last_frame: std::time::Instant::now(),
            hidpi_factor,
            state,
            transmitter,
            receiver,
            recorder: None,
            last_movie_frame_time: None,
        }
    }

    /// Renders the UI and responds to UI events.
    pub fn render_dashboard(&mut self) {
        let now = std::time::Instant::now();
        self.imgui_context
            .io_mut()
            .update_delta_time(now - self.last_frame);
        self.last_frame = now;

        let frame = match self.swap_chain.get_current_frame() {
            Ok(frame) => frame,
            Err(e) => {
                warn!("GUI Dropped frame: {:?}", e);
                return;
            }
        };
        self.imgui_platform
            .prepare_frame(self.imgui_context.io_mut(), &self.window)
            .expect("Failed to prepare frame");

        let ui = self.imgui_context.frame();
        let mut color_tokens = vec![];
        color_tokens.push(ui.push_style_color(StyleColor::Text, [0.0, 0.0, 0.0, 1.0]));
        color_tokens.push(ui.push_style_color(StyleColor::Header, [0.949, 0.949, 0.953, 1.0]));
        color_tokens.push(ui.push_style_color(StyleColor::HeaderHovered, [1.0, 1.0, 1.0, 1.0]));
        color_tokens.push(ui.push_style_color(StyleColor::Button, [0.741, 0.933, 0.984, 1.0]));
        color_tokens
            .push(ui.push_style_color(StyleColor::ButtonActive, [0.741, 0.933, 0.984, 1.0]));
        color_tokens
            .push(ui.push_style_color(StyleColor::ButtonHovered, [0.533, 0.851, 0.816, 1.0]));
        color_tokens.push(ui.push_style_color(StyleColor::FrameBg, [0.741, 0.933, 0.984, 1.0]));
        color_tokens.push(ui.push_style_color(StyleColor::WindowBg, [0.906, 0.784, 0.573, 1.0]));

        {
            let render_time = self.state.last_render_time;
            let frame_num = self.state.frame_num;
            let frame_timeouts = self.state.frame_timeout_count;
            let mouse_pos = self.state.mouse_pos;
            let render_canvas_size = self.state.render_window_size;
            let paused_state = self.state.paused;
            let mut pause_button_pressed = false;
            let titlebars_state = self.state.show_titlebar;
            let mut titlebar_button_pressed = false;
            let gui_width = self.size.width as f32 / self.hidpi_factor;
            let mut create_painting_button_pressed = false;
            let painting_width = &mut self.state.painting_resolution.x;
            let painting_height = &mut self.state.painting_resolution.y;
            let recording_width = &mut self.state.recording_resolution.x;
            let recording_height = &mut self.state.recording_resolution.y;
            let movie_framerate = &mut self.state.movie_framerate;
            let mut painting_filename = ImString::with_capacity(256);
            let mut recording_filename = ImString::with_capacity(256);
            let open_painting_externally = &mut self.state.open_painting_externally;
            let pause_while_painting = &mut self.state.pause_while_painting;
            let shader_compilation_error_msg = self.state.shader_compilation_error_msg.as_ref();
            let user_uniforms = &mut self.state.gui_uniforms;
            let mut record_button_pressed = false;
            let recorder = self.recorder.as_ref();

            painting_filename.push_str(&self.state.painting_filename);
            recording_filename.push_str(&self.state.recording_filename);
            let mut painting_filename_changed = false;
            let mut recording_filename_changed = false;
            let painting_in_progress = match &mut self.state.painting_progress_receiver {
                None => false,
                Some(rx) => {
                    let msg_result = rx.try_recv();
                    match msg_result {
                        Ok(_) => {
                            self.state.painting_progress_receiver = None;

                            // Log the amount of time render + write took.
                            if let Some(start) = self.state.painting_start_time {
                                let now = std::time::Instant::now();
                                let elapsed = now.duration_since(start).as_secs_f64();
                                info!("Painting render + write took {} seconds", elapsed);
                                self.state.painting_start_time = None;
                            }

                            // Send message to unpause the rendering.
                            if *pause_while_painting {
                                self.transmitter.send(DashboardMessage::Play).unwrap();
                            }
                            false
                        } // Finished.
                        Err(_) => true, // Still writing, hasn't reported status yet.
                    }
                }
            };
            let controls = imgui::Window::new(im_str!("Controls"));

            controls
                .size(
                    [
                        self.window.inner_size().width as f32 / self.hidpi_factor,
                        self.window.inner_size().height as f32 / self.hidpi_factor,
                    ],
                    Condition::Always,
                )
                .position([0.0, 0.0], Condition::Always)
                .collapsible(false)
                .no_decoration()
                .movable(false)
                .build(&ui, || {
                    if imgui::CollapsingHeader::new(im_str!("Stats & Controls"))
                        .default_open(true)
                        .open_on_arrow(true)
                        .open_on_double_click(true)
                        .build(&ui)
                    {
                        ui.text(format!("Render Time: {:.3} ms", render_time));
                        ui.text(format!("Frames Rendered: {}", frame_num));
                        ui.text(format!("Frame Timeouts: {}", frame_timeouts));
                        ui.text(im_str!(
                            "Mouse Position: ({:.1}, {:.1})",
                            mouse_pos.x,
                            mouse_pos.y
                        ));
                        ui.text(im_str!(
                            "Canvas Size: {} x {}",
                            render_canvas_size.x,
                            render_canvas_size.y
                        ));
                        ui.separator();
                        if paused_state {
                            pause_button_pressed = ui.button(im_str!("Play"), [gui_width, 25.0]);
                        } else {
                            pause_button_pressed = ui.button(im_str!("Pause"), [gui_width, 25.0]);
                        }
                        if titlebars_state {
                            titlebar_button_pressed =
                                ui.button(im_str!("Hide Titlebar"), [gui_width, 25.0]);
                        } else {
                            titlebar_button_pressed =
                                ui.button(im_str!("Show Titlebar"), [gui_width, 25.0]);
                        }
                    }

                    if imgui::CollapsingHeader::new(im_str!("Painting Options"))
                        .default_open(true)
                        .open_on_arrow(true)
                        .open_on_double_click(true)
                        .build(&ui)
                    {
                        ui.input_int(im_str!("Width##Painting"), painting_width)
                            .build();
                        ui.input_int(im_str!("Height##Painting"), painting_height)
                            .build();

                        let file_input =
                            ui.input_text(im_str!("Filename##Painting"), &mut painting_filename);
                        painting_filename_changed = file_input.build();
                        if cfg!(target_os = "macos") {
                            ui.checkbox(
                                im_str!("Open Painting in External App"),
                                open_painting_externally,
                            );
                        }
                        ui.checkbox(im_str!("Pause While Painting"), pause_while_painting);
                        if !painting_in_progress {
                            create_painting_button_pressed =
                                ui.button(im_str!("Create"), [gui_width, 50.0]);
                        }
                    }

                    if imgui::CollapsingHeader::new(im_str!("Recording Options"))
                        .default_open(true)
                        .open_on_arrow(true)
                        .open_on_double_click(true)
                        .build(&ui)
                    {
                        ui.input_int(im_str!("Width##Movie"), recording_width)
                            .build();
                        ui.input_int(im_str!("Height##Movie"), recording_height)
                            .build();
                        ui.input_int(im_str!("Framerate##Movie"), movie_framerate)
                            .build();

                        let file_input =
                            ui.input_text(im_str!("Filename##Movie"), &mut recording_filename);
                        recording_filename_changed = file_input.build();
                        if let Some(rec) = recorder {
                            if !rec.stop_signal_sent {
                                record_button_pressed =
                                    ui.button(im_str!("Stop##Recording"), [gui_width, 25.0]);
                            }
                        } else {
                            record_button_pressed =
                                ui.button(im_str!("Start##Recording"), [gui_width, 25.0]);
                        }
                    }
                    //---------------------------------
                    if !user_uniforms.is_empty() {
                        if imgui::CollapsingHeader::new(im_str!("Uniforms"))
                            .default_open(true)
                            .open_on_arrow(true)
                            .open_on_double_click(true)
                            .build(&ui)
                        {
                            for uniform in user_uniforms {
                                uniforms::update_user_uniform_ui(&ui, uniform);
                            }
                        }
                    }
                    //---------------------------------
                    ui.popup_modal(im_str!("Shader Recompilation")).build(|| {
                        if shader_compilation_error_msg.is_none() {
                            ui.close_current_popup();
                        }
                        ui.text_colored(
                            [1.0, 0.325, 0.286, 1.0],
                            im_str!("Error compiling shader."),
                        );
                        ui.text_wrapped(im_str!("See log for details."));
                    });
                    if shader_compilation_error_msg.is_some() {
                        ui.open_popup(im_str!("Shader Recompilation"));
                    }
                });
            if pause_button_pressed {
                self.state.paused = !self.state.paused;
                self.transmitter
                    .send(DashboardMessage::PausePlayChanged)
                    .unwrap();
            }
            if titlebar_button_pressed {
                self.state.show_titlebar = !self.state.show_titlebar;
                self.transmitter
                    .send(DashboardMessage::TitlebarStatusChanged)
                    .unwrap();
            }
            if painting_filename_changed {
                self.state.painting_filename = String::from(painting_filename.to_str());
            }
            if create_painting_button_pressed {
                if *pause_while_painting {
                    self.transmitter.send(DashboardMessage::Pause).unwrap();
                }
                self.transmitter
                    .send(DashboardMessage::PaintingRenderRequested(UIntVector2::new(
                        self.state.painting_resolution.x as u32,
                        self.state.painting_resolution.y as u32,
                    )))
                    .unwrap();
            }
            if recording_filename_changed {
                self.state.recording_filename = String::from(recording_filename.to_str());
            }
            if record_button_pressed {
                if self.recorder.is_none() {
                    self.recorder = Some(Recorder::new(
                        self.state.recording_resolution.x as u32,
                        self.state.recording_resolution.y as u32,
                        MOVIE_TEXTURE_FORMAT,
                        *movie_framerate as u32,
                        format!("{}.mp4", self.state.recording_filename),
                    ));
                } else {
                    let recorder = self.recorder.as_mut().unwrap();
                    recorder.stop();
                }
            }
        }

        while !color_tokens.is_empty() {
            let token = color_tokens.pop().unwrap();
            token.pop(&ui);
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("GUI Command Encoder"),
            });
        self.imgui_platform.prepare_render(&ui, &self.window);

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[wgpu::RenderPassColorAttachmentDescriptor {
                    attachment: &frame.output.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear_color),
                        store: true,
                    },
                }],
                depth_stencil_attachment: None,
            });

            self.imgui_renderer
                .render(ui.render(), &self.queue, &self.device, &mut rpass)
                .expect("GUI Rendering Failed");
        }

        self.queue.submit(Some(encoder.finish()));
    }

    /// Receives events from the winit event queue and responds appropriately.
    pub fn input(&mut self, event: &winit::event::Event<()>) {
        match event {
            Event::WindowEvent {
                ref event,
                window_id,
            } if *window_id == self.window.id() => match event {
                WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                    self.hidpi_factor = *scale_factor as f32;
                }
                WindowEvent::Resized(physical_size) => {
                    self.size = *physical_size;
                    self.sc_desc = wgpu::SwapChainDescriptor {
                        usage: wgpu::TextureUsage::RENDER_ATTACHMENT,
                        format: wgpu::TextureFormat::Bgra8UnormSrgb,
                        width: physical_size.width as u32,
                        height: physical_size.height as u32,
                        present_mode: wgpu::PresentMode::Mailbox,
                    };
                    self.swap_chain = self.device.create_swap_chain(&self.surface, &self.sc_desc);
                }
                WindowEvent::KeyboardInput { input, .. } => match input {
                    KeyboardInput {
                        state: ElementState::Pressed,
                        virtual_keycode: Some(VirtualKeyCode::Space),
                        ..
                    } => {
                        self.state.paused = !self.state.paused;
                        self.transmitter
                            .send(DashboardMessage::PausePlayChanged)
                            .unwrap();
                    }
                    _ => (),
                },
                _ => {}
            },
            _ => (),
        }
        self.imgui_platform
            .handle_event(self.imgui_context.io_mut(), &self.window, event);
    }

    /// Used to parse and respond to messages received from [crate::canvas::Canvas]
    fn handle_message(&mut self, message: CanvasMessage) {
        match message {
            CanvasMessage::FrameStep => self.state.frame_num += 1,
            CanvasMessage::MouseMoved(pos) => self.state.mouse_pos = pos,
            CanvasMessage::RenderPassSubmitted => {}
            CanvasMessage::WindowResized(new_size) => self.state.render_window_size = new_size,
            CanvasMessage::SwapChainFrameError(frame_error) => match frame_error {
                wgpu::SwapChainError::Timeout => self.state.frame_timeout_count += 1,
                _ => {}
            },
            CanvasMessage::PaintingStarted(buf, resolution, start_time) => {
                let filename = self.state.painting_filename.clone() + ".tiff";
                self.state.painting_start_time = Some(start_time);
                let open_externally = match cfg!(target_os = "macos") {
                    true => self.state.open_painting_externally,
                    false => false,
                };
                self.state.painting_progress_receiver = Some(AsyncTiffWriter::write(
                    buf,
                    UIntVector2::new(resolution.x as u32, resolution.y as u32),
                    filename,
                    open_externally,
                ));
            }
            CanvasMessage::ShaderCompilationFailed(err_msg) => {
                self.state.shader_compilation_error_msg = Some(err_msg);
                // Pause rendering
                self.transmitter.send(DashboardMessage::Pause).unwrap();
            }
            CanvasMessage::ShaderCompilationSucceeded => {
                self.state.shader_compilation_error_msg = None;
                self.transmitter.send(DashboardMessage::Play).unwrap();
                self.state.paused = false;
            }
            CanvasMessage::PausePlayChanged => {
                self.state.paused = !self.state.paused;
            }
            CanvasMessage::UniformForGUI(uniform) => {
                self.state.gui_uniforms.push(uniform);
            }
            CanvasMessage::UpdatePaintingResolutioninGUI(res) => {
                self.state.painting_resolution = res;
            }
            CanvasMessage::MovieFrameStarted(buf, resolution, start_time) => {
                if let Some(ref mut recorder) = self.recorder {
                    recorder.add_frame(buf, resolution, start_time);
                } else {
                    panic!("Frame received for movie at timestamp {:?}, but no recorder is instantiated.", start_time);
                }
            }
        }
    }

    /// Expected to be called every frame tick **before** [Self::render_dashboard()]
    /// Checks the receiver queue for any incoming messages, among other things.
    pub fn update(&mut self) {
        self.device.poll(wgpu::Maintain::Poll);
        let update_time = std::time::Instant::now();
        // First, check if we have received any messages and act accordingly
        loop {
            let msg_result = self.receiver.try_recv();
            match msg_result {
                Ok(msg) => self.handle_message(msg),
                Err(_) => break,
            }
        }

        if let Some(ref mut recorder) = self.recorder {
            if self.state.movie_framerate < 1 {
                panic!("Invalid framerate {} provided!", self.state.movie_framerate);
            }
            // If we have not stopped, keep requesting frames on the selected FPS interval
            let mut frame_needed = !recorder.stop_signal_sent;
            if let Some(last_frame_time) = self.last_movie_frame_time.as_mut() {
                let seconds_per_frame = 1.0 / (self.state.movie_framerate as f64);
                let delta = (update_time - *last_frame_time).as_secs_f64();
                frame_needed = frame_needed && delta >= seconds_per_frame;
            }
            if frame_needed {
                self.transmitter
                    .send(DashboardMessage::MovieRenderRequested(UIntVector2::new(
                        self.state.recording_resolution.x as u32,
                        self.state.recording_resolution.y as u32,
                    )))
                    .unwrap();
                self.last_movie_frame_time = Some(update_time);
            }
            // If finished, cleanup.
            if recorder.poll() {
                self.recorder.take().unwrap().finish();
            }
        }

        // Ping Canvas with the currently set painting res
        self.transmitter
            .send(DashboardMessage::PaintingResolutionUpdated(
                UIntVector2::new(
                    self.state.painting_resolution.x as u32,
                    self.state.painting_resolution.y as u32,
                ),
            ))
            .unwrap();
    }

    pub fn post_render(&mut self) {
        for uniform in &self.state.gui_uniforms {
            self.transmitter
                .send(DashboardMessage::UniformUpdatedViaGUI(uniform.copy()))
                .unwrap();
        }
        self.state.gui_uniforms.clear();
        let now = std::time::Instant::now();
        self.state.last_render_time = (now - self.last_frame).as_secs_f64() * 1000.0;
        self.window.request_redraw();
        self.last_frame = now;
    }
}
