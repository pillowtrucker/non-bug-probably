use std::{
    collections::HashMap, future::Future, hash::BuildHasher, path::Path, process::exit, sync::Arc,
    time::Duration,
};

use glam::{uvec2, vec2, DVec2, Mat3A, Mat4, UVec2, Vec2, Vec3, Vec3A};
use inox2d::formats::inp::parse_inp;
use log::{info, logger, warn};
use pico_args::Arguments;
use rend3::{
    types::{
        Backend, Camera, CameraProjection, DirectionalLight, DirectionalLightHandle, Handedness,
        SampleCount, Texture, TextureFormat,
    },
    util::typedefs::FastHashMap,
    Renderer, RendererProfile,
};
use rend3_framework::{lock, App as _, AssetPath, Event, Mutex, UserResizeEvent};
use rend3_gltf::GltfSceneInstance;
use rend3_routine::{base::BaseRenderGraph, pbr::NormalTextureYDirection, skybox::SkyboxRoutine};
use web_time::Instant;
use wgpu::{Extent3d, Features, Surface};
use wgpu_profiler::GpuTimerScopeResult;
#[cfg(target_arch = "wasm32")]
use winit::keyboard::PhysicalKey::Code;
#[cfg(not(target_arch = "wasm32"))]
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::{
    event::{DeviceEvent, ElementState, KeyEvent, MouseButton, WindowEvent},
    event_loop::EventLoopWindowTarget,
    window::{Fullscreen, Window, WindowBuilder},
};

mod platform;

async fn load_skybox_image(loader: &rend3_framework::AssetLoader, data: &mut Vec<u8>, path: &str) {
    let decoded = image::load_from_memory(
        &loader
            .get_asset(AssetPath::Internal(path))
            .await
            .unwrap_or_else(|e| panic!("Error {}: {}", path, e)),
    )
    .unwrap()
    .into_rgba8();

    data.extend_from_slice(decoded.as_raw());
}

async fn load_skybox(
    renderer: &Arc<Renderer>,
    loader: &rend3_framework::AssetLoader,
    skybox_routine: &Mutex<SkyboxRoutine>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut data = Vec::new();
    load_skybox_image(loader, &mut data, "skybox/right.jpg").await;
    load_skybox_image(loader, &mut data, "skybox/left.jpg").await;
    load_skybox_image(loader, &mut data, "skybox/top.jpg").await;
    load_skybox_image(loader, &mut data, "skybox/bottom.jpg").await;
    load_skybox_image(loader, &mut data, "skybox/front.jpg").await;
    load_skybox_image(loader, &mut data, "skybox/back.jpg").await;

    let handle = renderer.add_texture_cube(Texture {
        format: TextureFormat::Bgra8Unorm,
        size: UVec2::new(2048, 2048),
        data,
        label: Some("background".into()),
        mip_count: rend3::types::MipmapCount::ONE,
        mip_source: rend3::types::MipmapSource::Uploaded,
    })?;
    lock(skybox_routine).set_background_texture(Some(handle));
    Ok(())
}

async fn load_gltf(
    renderer: &Arc<Renderer>,
    loader: &rend3_framework::AssetLoader,
    settings: &rend3_gltf::GltfLoadSettings,
    location: AssetPath<'_>,
) -> Option<(rend3_gltf::LoadedGltfScene, GltfSceneInstance)> {
    // profiling::scope!("loading gltf");
    let gltf_start = Instant::now();
    let is_default_scene = matches!(location, AssetPath::Internal(_));
    let path = loader.get_asset_path(location);
    let path = Path::new(&*path);
    let parent = path.parent().unwrap();

    let parent_str = parent.to_string_lossy();
    let path_str = path.as_os_str().to_string_lossy();
    log::info!("Reading gltf file: {}", path_str);
    let gltf_data_result = loader.get_asset(AssetPath::External(&path_str)).await;

    let gltf_data = match gltf_data_result {
        Ok(d) => d,
        Err(_) if is_default_scene => {
            let suffix = if cfg!(target_os = "windows") {
                ".exe"
            } else {
                ""
            };

            indoc::eprintdoc!("
                *** WARNING ***

                It appears you are running scene-viewer with no file to display.
                
                The default scene is no longer bundled into the repository. If you are running on git, use the following commands
                to download and unzip it into the right place. If you're running it through not-git, pass a custom folder to the -C argument
                to tar, then run scene-viewer path/to/scene.gltf.
                
                curl{0} https://cdn.cwfitz.com/scenes/rend3-default-scene.tar -o ./examples/scene-viewer/resources/rend3-default-scene.tar
                tar{0} xf ./examples/scene-viewer/resources/rend3-default-scene.tar -C ./examples/scene-viewer/resources

                ***************
            ", suffix);

            return None;
        }
        e => e.unwrap(),
    };

    let gltf_elapsed = gltf_start.elapsed();
    let resources_start = Instant::now();
    let (scene, instance) = rend3_gltf::load_gltf(renderer, &gltf_data, settings, |uri| async {
        if let Some(base64) = rend3_gltf::try_load_base64(&uri) {
            Ok(base64)
        } else {
            log::info!("Loading resource {}", uri);
            let uri = uri;
            let full_uri = parent_str.clone() + "/" + uri.as_str();
            loader.get_asset(AssetPath::External(&full_uri)).await
        }
    })
    .await
    .unwrap();

    log::info!(
        "Loaded gltf in {:.3?}, resources loaded in {:.3?}",
        gltf_elapsed,
        resources_start.elapsed()
    );
    Some((scene, instance))
}

fn button_pressed<Hash: BuildHasher>(map: &HashMap<u32, bool, Hash>, key: u32) -> bool {
    map.get(&key).map_or(false, |b| *b)
}

fn extract_backend(value: &str) -> Result<Backend, &'static str> {
    Ok(match value.to_lowercase().as_str() {
        "vulkan" | "vk" => Backend::Vulkan,
        "dx12" | "12" => Backend::Dx12,
        "dx11" | "11" => Backend::Dx11,
        "metal" | "mtl" => Backend::Metal,
        "opengl" | "gl" => Backend::Gl,
        _ => return Err("unknown backend"),
    })
}

fn extract_profile(value: &str) -> Result<rend3::RendererProfile, &'static str> {
    Ok(match value.to_lowercase().as_str() {
        "legacy" | "c" | "cpu" => rend3::RendererProfile::CpuDriven,
        "modern" | "g" | "gpu" => rend3::RendererProfile::GpuDriven,
        _ => return Err("unknown rendermode"),
    })
}

fn extract_msaa(value: &str) -> Result<SampleCount, &'static str> {
    Ok(match value {
        "1" => SampleCount::One,
        "4" => SampleCount::Four,
        _ => return Err("invalid msaa count"),
    })
}

fn extract_vsync(value: &str) -> Result<rend3::types::PresentMode, &'static str> {
    Ok(match value.to_lowercase().as_str() {
        "immediate" => rend3::types::PresentMode::Immediate,
        "fifo" => rend3::types::PresentMode::Fifo,
        "mailbox" => rend3::types::PresentMode::Mailbox,
        _ => return Err("invalid msaa count"),
    })
}

fn extract_array<const N: usize>(value: &str, default: [f32; N]) -> Result<[f32; N], &'static str> {
    let mut res = default;
    let split: Vec<_> = value.split(',').enumerate().collect();

    if split.len() != N {
        return Err("Mismatched argument count");
    }

    for (idx, inner) in split {
        let inner = inner.trim();

        res[idx] = inner.parse().map_err(|_| "Cannot parse argument number")?;
    }
    Ok(res)
}

fn extract_vec3(value: &str) -> Result<Vec3, &'static str> {
    let mut res = [0.0_f32, 0.0, 0.0];
    let split: Vec<_> = value.split(',').enumerate().collect();

    if split.len() != 3 {
        return Err("Directional lights are defined with 3 values");
    }

    for (idx, inner) in split {
        let inner = inner.trim();

        res[idx] = inner.parse().map_err(|_| "Cannot parse direction number")?;
    }
    Ok(Vec3::from(res))
}

fn option_arg<T>(result: Result<Option<T>, pico_args::Error>) -> Option<T> {
    match result {
        Ok(o) => o,
        Err(pico_args::Error::Utf8ArgumentParsingFailed { value, cause }) => {
            eprintln!("{}: '{}'\n\n{}", cause, value, HELP);
            std::process::exit(1);
        }
        Err(pico_args::Error::OptionWithoutAValue(value)) => {
            eprintln!("{} flag needs an argument", value);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("{:?}", e);
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn<Fut>(fut: Fut)
where
    Fut: Future + Send + 'static,
    Fut::Output: Send + 'static,
{
    std::thread::spawn(|| pollster::block_on(fut));
}

#[cfg(target_arch = "wasm32")]
pub fn spawn<Fut>(fut: Fut)
where
    Fut: Future + 'static,
    Fut::Output: 'static,
{
    wasm_bindgen_futures::spawn_local(async move {
        fut.await;
    });
}

const HELP: &str = "\
scene-viewer

gltf and glb scene viewer powered by the rend3 rendering library.

usage: scene-viewer --options ./path/to/gltf/file.gltf

Meta:
  --help            This menu.

Rendering:
  -b --backend                 Choose backend to run on ('vk', 'dx12', 'dx11', 'metal', 'gl').
  -d --device                  Choose device to run on (case insensitive device substring).
  -p --profile                 Choose rendering profile to use ('cpu', 'gpu').
  -v --vsync                   Choose vsync mode ('immediate' [no-vsync], 'fifo' [vsync], 'fifo_relaxed' [adaptive vsync], 'mailbox' [fast vsync])
  --msaa <level>               Level of antialiasing (either 1 or 4). Default 1.

Windowing:
  --absolute-mouse             Interpret the relative mouse coordinates as absolute. Useful when using things like VNC.
  --fullscreen                 Open the window in borderless fullscreen.

Assets:
  --normal-y-down                        Interpret all normals as having the DirectX convention of Y down. Defaults to Y up.
  --directional-light <x,y,z>            Create a directional light pointing towards the given coordinates.
  --directional-light-intensity <value>  All lights created by the above flag have this intensity. Defaults to 4.
  --gltf-disable-directional-lights      Disable all directional lights in the gltf
  --ambient <value>                      Set the value of the minimum ambient light. This will be treated as white light of this intensity. Defaults to 0.1.
  --scale <scale>                        Scale all objects loaded by this factor. Defaults to 1.0.
  --shadow-distance <value>              Distance from the camera there will be directional shadows. Lower values means higher quality shadows. Defaults to 100.
  --shadow-resolution <value>            Resolution of the shadow map. Higher values mean higher quality shadows with high performance cost. Defaults to 2048.

Controls:
  --walk <speed>               Walk speed (speed without holding shift) in units/second (typically meters). Default 10.
  --run  <speed>               Run speed (speed while holding shift) in units/second (typically meters). Default 50.
  --camera x,y,z,pitch,yaw     Spawns the camera at the given position. Press Period to get the current camera position.
--puppet <path>                path to .inp
";

struct SceneViewer {
    absolute_mouse: bool,
    desired_backend: Option<Backend>,
    desired_device_name: Option<String>,
    desired_profile: Option<RendererProfile>,
    file_to_load: Option<String>,
    walk_speed: f32,
    run_speed: f32,
    gltf_settings: rend3_gltf::GltfLoadSettings,
    directional_light_direction: Option<Vec3>,
    directional_light_intensity: f32,
    directional_light: Option<DirectionalLightHandle>,
    ambient_light_level: f32,
    present_mode: rend3::types::PresentMode,
    samples: SampleCount,

    fullscreen: bool,

    scancode_status: FastHashMap<u32, bool>,
    camera_pitch: f32,
    camera_yaw: f32,
    camera_location: Vec3A,
    previous_profiling_stats: Option<Vec<GpuTimerScopeResult>>,
    timestamp_last_second: Instant,
    timestamp_last_frame: Instant,
    timestamp_start: Instant,
    frame_times: histogram::Histogram,
    last_mouse_delta: Option<DVec2>,

    grabber: Option<rend3_framework::Grabber>,
    inox_model: inox2d::model::Model,
    inox_renderer: Option<inox2d_wgpu::Renderer>,
    inox_texture: Option<wgpu::Texture>,
}
impl SceneViewer {
    pub fn new() -> Self {
        #[cfg(feature = "tracy")]
        tracy_client::Client::start();
        let timestamp_start = Instant::now();
        let mut args = Arguments::from_vec(std::env::args_os().skip(1).collect());

        // Meta
        let help = args.contains(["-h", "--help"]);

        // Rendering
        let desired_backend =
            option_arg(args.opt_value_from_fn(["-b", "--backend"], extract_backend));
        let desired_device_name: Option<String> =
            option_arg(args.opt_value_from_str(["-d", "--device"]))
                .map(|s: String| s.to_lowercase());
        let desired_mode = option_arg(args.opt_value_from_fn(["-p", "--profile"], extract_profile));
        let samples =
            option_arg(args.opt_value_from_fn("--msaa", extract_msaa)).unwrap_or(SampleCount::One);
        let present_mode = option_arg(args.opt_value_from_fn(["-v", "--vsync"], extract_vsync))
            .unwrap_or(rend3::types::PresentMode::Immediate);

        // Windowing
        let absolute_mouse: bool = args.contains("--absolute-mouse");
        let fullscreen = args.contains("--fullscreen");
        let puppet =
            option_arg(args.opt_value_from_str("--puppet")).unwrap_or("Midori.inp".to_owned());
        // Assets
        let normal_direction = match args.contains("--normal-y-down") {
            true => NormalTextureYDirection::Down,
            false => NormalTextureYDirection::Up,
        };
        let directional_light_direction =
            option_arg(args.opt_value_from_fn("--directional-light", extract_vec3));
        let directional_light_intensity: f32 =
            option_arg(args.opt_value_from_str("--directional-light-intensity")).unwrap_or(4.0);
        let ambient_light_level: f32 =
            option_arg(args.opt_value_from_str("--ambient")).unwrap_or(0.10);
        let scale: Option<f32> = option_arg(args.opt_value_from_str("--scale"));
        let shadow_distance: Option<f32> = option_arg(args.opt_value_from_str("--shadow-distance"));
        let shadow_resolution: Option<u16> =
            option_arg(args.opt_value_from_str("--shadow-resolution"));
        let gltf_disable_directional_light: bool =
            args.contains("--gltf-disable-directional-lights");

        // Controls
        let walk_speed = args.value_from_str("--walk").unwrap_or(10.0_f32);
        let run_speed = args.value_from_str("--run").unwrap_or(50.0_f32);
        let camera_default = [
            3.0,
            3.0,
            3.0,
            -std::f32::consts::FRAC_PI_8,
            std::f32::consts::FRAC_PI_4,
        ];
        let camera_info = args
            .value_from_str("--camera")
            .map_or(camera_default, |s: String| {
                extract_array(&s, camera_default).unwrap()
            });

        // Free args
        let file_to_load: Option<String> =
            Some(args.free_from_str().unwrap_or("LinacLab.glb".to_owned()));

        let remaining = args.finish();

        if !remaining.is_empty() {
            eprint!("Unknown arguments:");
            for flag in remaining {
                eprint!(" '{}'", flag.to_string_lossy());
            }
            eprintln!("\n");

            eprintln!("{}", HELP);
            std::process::exit(1);
        }

        if help {
            eprintln!("{}", HELP);
            std::process::exit(1);
        }

        let mut gltf_settings = rend3_gltf::GltfLoadSettings {
            normal_direction,
            enable_directional: !gltf_disable_directional_light,
            ..Default::default()
        };
        if let Some(scale) = scale {
            gltf_settings.scale = scale
        }
        if let Some(shadow_distance) = shadow_distance {
            gltf_settings.directional_light_shadow_distance = shadow_distance;
        }
        if let Some(shadow_resolution) = shadow_resolution {
            gltf_settings.directional_light_resolution = shadow_resolution;
        }
        let inox_model = parse_inp(
            pollster::block_on(async {
                let loader = rend3_framework::AssetLoader::new_local(
                    concat!(env!("CARGO_MANIFEST_DIR"), "/"),
                    "",
                    "http://localhost:8000/",
                );
                loader
                    .get_asset(AssetPath::Internal(&puppet))
                    .await
                    .unwrap()
            })
            .as_slice(),
        )
        .unwrap();

        Self {
            absolute_mouse,
            desired_backend,
            desired_device_name,
            desired_profile: desired_mode,
            file_to_load,
            inox_renderer: None,
            inox_model,
            walk_speed,
            run_speed,
            gltf_settings,
            directional_light_direction,
            directional_light_intensity,
            directional_light: None,
            ambient_light_level,
            present_mode,
            samples,
            timestamp_start,
            fullscreen,
            inox_texture: None,
            scancode_status: FastHashMap::default(),
            camera_pitch: camera_info[3],
            camera_yaw: camera_info[4],
            camera_location: Vec3A::new(camera_info[0], camera_info[1], camera_info[2]),
            previous_profiling_stats: None,
            timestamp_last_second: Instant::now(),
            timestamp_last_frame: Instant::now(),
            frame_times: histogram::Histogram::new(),
            last_mouse_delta: None,

            grabber: None,
        }
    }
}
impl rend3_framework::App for SceneViewer {
    const HANDEDNESS: rend3::types::Handedness = rend3::types::Handedness::Right;

    fn create_window(
        &mut self,
        builder: WindowBuilder,
    ) -> Result<
        (
            winit::event_loop::EventLoop<UserResizeEvent<()>>,
            winit::window::Window,
        ),
        winit::error::EventLoopError,
    > {
        profiling::scope!("creating window");

        let event_loop = winit::event_loop::EventLoopBuilder::with_user_event().build()?;
        let window = builder.build(&event_loop).expect("Could not build window");

        #[cfg(target_arch = "wasm32")]
        {
            use winit::platform::web::WindowExtWebSys;

            let canvas = window.canvas().unwrap();
            let style = canvas.style();
            style.set_property("width", "100%").unwrap();
            style.set_property("height", "100%").unwrap();

            web_sys::window()
                .and_then(|win| win.document())
                .and_then(|doc| doc.body())
                .and_then(|body| body.append_child(&canvas).ok())
                .expect("couldn't append canvas to document body");
        }

        Ok((event_loop, window))
    }

    fn create_iad<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<rend3::InstanceAdapterDevice>> + 'a>,
    > {
        Box::pin(async move {
            Ok(rend3::create_iad(
                self.desired_backend,
                self.desired_device_name.clone(),
                self.desired_profile,
                Some(Features::ADDRESS_MODE_CLAMP_TO_BORDER),
            )
            .await?)
        })
    }

    fn create_base_rendergraph(
        &mut self,
        renderer: &Arc<Renderer>,
        spp: &rend3::ShaderPreProcessor,
    ) -> BaseRenderGraph {
        BaseRenderGraph::new(renderer, spp)
    }

    fn sample_count(&self) -> SampleCount {
        self.samples
    }

    fn present_mode(&self) -> rend3::types::PresentMode {
        self.present_mode
    }

    fn scale_factor(&self) -> f32 {
        // Android has very low memory bandwidth, so lets run internal buffers at half
        // res by default
        cfg_if::cfg_if! {
            if #[cfg(target_os = "android")] {
                0.5
            } else {
                1.0
            }
        }
    }

    fn setup<'a>(
        &'a mut self,
        _event_loop: &winit::event_loop::EventLoop<rend3_framework::UserResizeEvent<()>>,
        window: &'a winit::window::Window,
        renderer: &'a Arc<Renderer>,
        routines: &'a Arc<rend3_framework::DefaultRoutines>,
        _surface_format: rend3::types::TextureFormat,
    ) {
        self.grabber = Some(rend3_framework::Grabber::new(window));

        if let Some(direction) = self.directional_light_direction {
            self.directional_light = Some(renderer.add_directional_light(DirectionalLight {
                color: Vec3::splat(1.0),
                intensity: self.directional_light_intensity,
                direction,
                distance: self.gltf_settings.directional_light_shadow_distance,
                resolution: 2048,
            }));
        }

        let gltf_settings = self.gltf_settings;
        let file_to_load = self.file_to_load.take();
        let renderer = Arc::clone(renderer);
        let routines = Arc::clone(routines);
        let mut inox_renderer = inox2d_wgpu::Renderer::new(
            &renderer.device,
            &renderer.queue,
            wgpu::TextureFormat::Bgra8Unorm,
            &self.inox_model,
            uvec2(window.inner_size().width, window.inner_size().height),
        );
        inox_renderer.camera.scale = Vec2::splat(0.12);
        self.inox_renderer = Some(inox_renderer);

        let inox_texture = renderer.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("inox texture"),
            size: Extent3d {
                width: window.inner_size().width,
                height: window.inner_size().height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[wgpu::TextureFormat::Bgra8Unorm],
        });
        self.inox_texture = Some(inox_texture);
        spawn(async move {
            let loader = rend3_framework::AssetLoader::new_local(
                concat!(env!("CARGO_MANIFEST_DIR"), "/resources/"),
                "",
                "http://localhost:8000/resources/",
            );
            if let Err(e) = load_skybox(&renderer, &loader, &routines.skybox).await {
                println!("Failed to load skybox {}", e)
            };
            Box::leak(Box::new(
                load_gltf(
                    &renderer,
                    &loader,
                    &gltf_settings,
                    file_to_load.as_deref().map_or_else(
                        || AssetPath::Internal("default-scene/scene.gltf"),
                        AssetPath::External,
                    ),
                )
                .await,
            ));
        });
    }

    fn handle_event(
        &mut self,
        window: &winit::window::Window,
        renderer: &Arc<rend3::Renderer>,
        routines: &Arc<rend3_framework::DefaultRoutines>,
        base_rendergraph: &BaseRenderGraph,
        surface: Option<&Arc<rend3::types::Surface>>,
        resolution: UVec2,
        event: rend3_framework::Event<'_, ()>,
        _control_flow: impl FnOnce(winit::event_loop::ControlFlow),
        event_loop_window_target: &EventLoopWindowTarget<UserResizeEvent<()>>,
    ) {
        match event {
            Event::AboutToWait => {
                profiling::scope!("MainEventsCleared");
                let now = Instant::now();

                let delta_time = now - self.timestamp_last_frame;
                self.frame_times
                    .increment(delta_time.as_micros() as u64)
                    .unwrap();

                let elapsed_since_second = now - self.timestamp_last_second;
                if elapsed_since_second > Duration::from_secs(1) {
                    let count = self.frame_times.entries();
                    println!(
                        "{:0>5} frames over {:0>5.2}s. \
                        Min: {:0>5.2}ms; \
                        Average: {:0>5.2}ms; \
                        95%: {:0>5.2}ms; \
                        99%: {:0>5.2}ms; \
                        Max: {:0>5.2}ms; \
                        StdDev: {:0>5.2}ms",
                        count,
                        elapsed_since_second.as_secs_f32(),
                        self.frame_times.minimum().unwrap() as f32 / 1_000.0,
                        self.frame_times.mean().unwrap() as f32 / 1_000.0,
                        self.frame_times.percentile(95.0).unwrap() as f32 / 1_000.0,
                        self.frame_times.percentile(99.0).unwrap() as f32 / 1_000.0,
                        self.frame_times.maximum().unwrap() as f32 / 1_000.0,
                        self.frame_times.stddev().unwrap() as f32 / 1_000.0,
                    );
                    self.timestamp_last_second = now;
                    self.frame_times.clear();
                }

                self.timestamp_last_frame = now;

                let rotation = Mat3A::from_euler(
                    glam::EulerRot::XYZ,
                    -self.camera_pitch,
                    -self.camera_yaw,
                    0.0,
                )
                .transpose();
                let forward = -rotation.z_axis;
                let up = rotation.y_axis;
                let side = -rotation.x_axis;
                let velocity = if button_pressed(&self.scancode_status, platform::Scancodes::SHIFT)
                {
                    self.run_speed
                } else {
                    self.walk_speed
                };
                if button_pressed(&self.scancode_status, platform::Scancodes::W) {
                    self.camera_location += forward * velocity * delta_time.as_secs_f32();
                }
                if button_pressed(&self.scancode_status, platform::Scancodes::S) {
                    self.camera_location -= forward * velocity * delta_time.as_secs_f32();
                }
                if button_pressed(&self.scancode_status, platform::Scancodes::A) {
                    self.camera_location += side * velocity * delta_time.as_secs_f32();
                }
                if button_pressed(&self.scancode_status, platform::Scancodes::D) {
                    self.camera_location -= side * velocity * delta_time.as_secs_f32();
                }
                if button_pressed(&self.scancode_status, platform::Scancodes::Q) {
                    self.camera_location += up * velocity * delta_time.as_secs_f32();
                }
                if button_pressed(&self.scancode_status, platform::Scancodes::PERIOD) {
                    println!(
                        "{x},{y},{z},{pitch},{yaw}",
                        x = self.camera_location.x,
                        y = self.camera_location.y,
                        z = self.camera_location.z,
                        pitch = self.camera_pitch,
                        yaw = self.camera_yaw
                    );
                }

                if button_pressed(&self.scancode_status, platform::Scancodes::ESCAPE) {
                    self.grabber.as_mut().unwrap().request_ungrab(window);
                }

                if button_pressed(&self.scancode_status, platform::Scancodes::P) {
                    // write out gpu side performance info into a trace readable by chrome://tracing
                    if let Some(ref stats) = self.previous_profiling_stats {
                        println!("Outputing gpu timing chrome trace to profile.json");
                        wgpu_profiler::chrometrace::write_chrometrace(
                            Path::new("profile.json"),
                            stats,
                        )
                        .unwrap();
                    } else {
                        println!("No gpu timing trace available, either timestamp queries are unsupported or not enough frames have elapsed yet!");
                    }
                }

                window.request_redraw()
            }
            Event::WindowEvent {
                event: winit::event::WindowEvent::RedrawRequested,
                ..
            } => {
                let view = Mat4::from_euler(
                    glam::EulerRot::XYZ,
                    -self.camera_pitch,
                    -self.camera_yaw,
                    0.0,
                );
                let view = view * Mat4::from_translation((-self.camera_location).into());

                renderer.set_camera_data(Camera {
                    projection: CameraProjection::Perspective {
                        vfov: 60.0,
                        near: 0.1,
                    },
                    view,
                });
                /*

                */
                // Get a frame
                let frame = surface.unwrap().get_current_texture().unwrap();
                // Lock all the routines
                let pbr_routine = lock(&routines.pbr);
                let mut skybox_routine = lock(&routines.skybox);
                let tonemapping_routine = lock(&routines.tonemapping);

                // Swap the instruction buffers so that our frame's changes can be processed.
                renderer.swap_instruction_buffers();
                // Evaluate our frame's world-change instructions
                let mut eval_output = renderer.evaluate_instructions();
                // Evaluate changes to routines.
                skybox_routine.evaluate(renderer);

                // Build a rendergraph
                let mut graph = rend3::graph::RenderGraph::new();

                let frame_handle = graph.add_imported_render_target(
                    &frame,
                    0..1,
                    0..1,
                    rend3::graph::ViewportRect::from_size(resolution),
                );
                // Add the default rendergraph
                /*
                                base_rendergraph.add_to_graph(
                                    &mut graph,
                                    &eval_output,
                                    &pbr_routine,
                                    Some(&skybox_routine),
                                    &tonemapping_routine,
                                    frame_handle,
                                    resolution,
                                    self.samples,
                                    Vec3::splat(self.ambient_light_level).extend(1.0),
                                    glam::Vec4::new(0.0, 0.0, 0.0, 1.0),
                                );
                */
                base_rendergraph.add_to_graph(
                    &mut graph,
                    rend3_routine::base::BaseRenderGraphInputs {
                        eval_output: &eval_output,
                        routines: rend3_routine::base::BaseRenderGraphRoutines {
                            pbr: &pbr_routine,
                            skybox: Some(&skybox_routine),
                            tonemapping: &tonemapping_routine,
                        },
                        target: rend3_routine::base::OutputRenderTarget {
                            handle: frame_handle,
                            resolution,
                            samples: self.samples,
                        },
                    },
                    rend3_routine::base::BaseRenderGraphSettings {
                        ambient_color: Vec3::splat(self.ambient_light_level).extend(1.0),
                        clear_color: glam::Vec4::new(0.0, 0.0, 0.0, 1.0),
                    },
                );
                // Dispatch a render using the built up rendergraph!
                self.previous_profiling_stats = graph.execute(renderer, &mut eval_output);

                {
                    let puppet = &mut self.inox_model.puppet;
                    puppet.begin_set_params();
                    let t = self.timestamp_start.elapsed().as_secs_f32();
                    puppet.set_param("Head:: Yaw-Pitch", vec2(t.cos(), t.sin()));
                    puppet.end_set_params();
                }
                if let Some(ref mut inox_texture) = self.inox_texture {
                    let temp_view =
                        inox_texture.create_view(&wgpu::TextureViewDescriptor::default());

                    if let Some(ref mut ir) = self.inox_renderer {
                        ir.render(
                            &renderer.queue,
                            &renderer.device,
                            &self.inox_model.puppet,
                            &temp_view,
                        )
                    };
                    /*
                                        let mut encoder =
                                            renderer
                                                .device
                                                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                                                    label: Some("Part Render Encoder"),
                                                });


                                            encoder.copy_texture_to_texture(
                                                inox_texture.as_image_copy(),
                                                frame.texture.as_image_copy(),
                                                frame.texture.size(),
                                            );
                                            renderer.queue.submit(std::iter::once(encoder.finish()));
                    */
                }
                frame.present();
                // mark the end of the frame for tracy/other profilers
                profiling::finish_frame!();
            }
            Event::WindowEvent {
                event: WindowEvent::Focused(focus),
                ..
            } => {
                if !focus {
                    self.grabber.as_mut().unwrap().request_ungrab(window);
                }
            }

            Event::WindowEvent {
                event:
                    WindowEvent::KeyboardInput {
                        event:
                            KeyEvent {
                                physical_key,
                                state,
                                ..
                            },
                        ..
                    },
                ..
            } => {
                #[cfg(not(target_arch = "wasm32"))]
                let scancode = PhysicalKeyExtScancode::to_scancode(physical_key).unwrap();
                #[cfg(target_arch = "wasm32")]
                let scancode = if let Code(kk) = physical_key {
                    kk as u32
                } else {
                    0
                };
                log::info!("WE scancode {:x}", scancode);
                self.scancode_status.insert(
                    scancode,
                    match state {
                        ElementState::Pressed => true,
                        ElementState::Released => false,
                    },
                );
            }

            Event::WindowEvent {
                event:
                    WindowEvent::MouseInput {
                        button: MouseButton::Left,
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                let grabber = self.grabber.as_mut().unwrap();

                if !grabber.grabbed() {
                    grabber.request_grab(window);
                }
            }
            Event::DeviceEvent {
                event:
                    DeviceEvent::MouseMotion {
                        delta: (delta_x, delta_y),
                        ..
                    },
                ..
            } => {
                if !self.grabber.as_ref().unwrap().grabbed() {
                    return;
                }

                const TAU: f32 = std::f32::consts::PI * 2.0;

                let mouse_delta = if self.absolute_mouse {
                    let prev = self.last_mouse_delta.replace(DVec2::new(delta_x, delta_y));
                    if let Some(prev) = prev {
                        (DVec2::new(delta_x, delta_y) - prev) / 4.0
                    } else {
                        return;
                    }
                } else {
                    DVec2::new(delta_x, delta_y)
                };

                self.camera_yaw -= (mouse_delta.x / 1000.0) as f32;
                self.camera_pitch -= (mouse_delta.y / 1000.0) as f32;
                if self.camera_yaw < 0.0 {
                    self.camera_yaw += TAU;
                } else if self.camera_yaw >= TAU {
                    self.camera_yaw -= TAU;
                }
                self.camera_pitch = self.camera_pitch.clamp(
                    -std::f32::consts::FRAC_PI_2 + 0.0001,
                    std::f32::consts::FRAC_PI_2 - 0.0001,
                )
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                event_loop_window_target.exit();
            }
            _ => {}
        }
    }
}
struct StoredSurfaceInfo {
    size: UVec2,
    scale_factor: f32,
    sample_count: SampleCount,
    present_mode: wgpu::PresentMode,
}

#[cfg_attr(
    target_os = "android",
    ndk_glue::main(backtrace = "on", logger(level = "debug"))
)]
pub fn main() {
    let app = SceneViewer::new();

    let mut builder = WindowBuilder::new()
        .with_title("scene-viewer")
        .with_maximized(true);
    if app.fullscreen {
        builder = builder.with_fullscreen(Some(Fullscreen::Borderless(None)));
    }
    {
        #[cfg(target_arch = "wasm32")]
        {
            wasm_bindgen_futures::spawn_local(async_start(app, builder));
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            pollster::block_on({
                let mut app = app;
                async move {
                    app.register_logger();
                    app.register_panic_hook();
                    let Ok((event_loop, window)) = app.create_window(builder.with_visible(false))
                    else {
                        exit(1)
                    };
                    let window_size = window.inner_size();
                    let iad = app.create_iad().await.unwrap();
                    let mut surface = if cfg!(target_os = "android") {
                        None
                    } else {
                        Some(Arc::new(
                            unsafe { iad.instance.create_surface(&window) }.unwrap(),
                        ))
                    };
                    let renderer = rend3::Renderer::new(
                        iad.clone(),
                        Handedness::Right,
                        Some(window_size.width as f32 / window_size.height as f32),
                    )
                    .unwrap();
                    let format = surface.as_ref().map_or(TextureFormat::Bgra8Unorm, |s| {
                        //                        let caps = s.get_capabilities(&iad.adapter);
                        let format = TextureFormat::Bgra8Unorm;
                        //                        let format = caps.formats[0];

                        // Configure the surface to be ready for rendering.
                        rend3::configure_surface(
                            s,
                            &iad.device,
                            format,
                            glam::UVec2::new(window_size.width, window_size.height),
                            rend3::types::PresentMode::Immediate,
                        );
                        let alpha_mode = wgpu::CompositeAlphaMode::Auto;
                        let config = wgpu::SurfaceConfiguration {
                            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                                | wgpu::TextureUsages::COPY_DST,
                            format: wgpu::TextureFormat::Bgra8Unorm,
                            width: window_size.width,
                            height: window_size.height,
                            present_mode: wgpu::PresentMode::Immediate,
                            alpha_mode,
                            view_formats: Vec::new(),
                        };
                        surface
                            .as_ref()
                            .unwrap()
                            .configure(&renderer.device, &config);

                        format
                    });
                    let mut spp = rend3::ShaderPreProcessor::new();
                    rend3_routine::builtin_shaders(&mut spp);
                    let base_rendergraph = app.create_base_rendergraph(&renderer, &spp);
                    let mut data_core = renderer.data_core.lock();
                    let routines = Arc::new(rend3_framework::DefaultRoutines {
                        pbr: Mutex::new(rend3_routine::pbr::PbrRoutine::new(
                            &renderer,
                            &mut data_core,
                            &spp,
                            &base_rendergraph.interfaces,
                            &base_rendergraph.gpu_culler.culling_buffer_map_handle,
                        )),
                        skybox: Mutex::new(rend3_routine::skybox::SkyboxRoutine::new(
                            &renderer,
                            &spp,
                            &base_rendergraph.interfaces,
                        )),
                        tonemapping: Mutex::new(
                            rend3_routine::tonemapping::TonemappingRoutine::new(
                                &renderer,
                                &spp,
                                &base_rendergraph.interfaces,
                                format,
                            ),
                        ),
                    });
                    drop(data_core);
                    app.setup(&event_loop, &window, &renderer, &routines, format);
                    #[cfg(target_arch = "wasm32")]
                    let _observer =
                        resize_observer::ResizeObserver::new(&window, event_loop.create_proxy());
                    window.set_visible(true);
                    let mut suspended = cfg!(target_os = "android");
                    let mut last_user_control_mode = winit::event_loop::ControlFlow::Poll;
                    let mut stored_surface_info = StoredSurfaceInfo {
                        size: glam::UVec2::new(window_size.width, window_size.height),
                        scale_factor: app.scale_factor(),
                        sample_count: app.sample_count(),
                        present_mode: app.present_mode(),
                    };
                    #[allow(clippy::let_unit_value)]
                    let _ = winit_run(event_loop, move |event, event_loop_window_target| {
                        let event = match event {
                            Event::UserEvent(UserResizeEvent::Resize { size, window_id }) => {
                                Event::WindowEvent {
                                    window_id,
                                    event: WindowEvent::Resized(size),
                                }
                            }
                            e => e,
                        };
                        let mut control_flow = event_loop_window_target.control_flow();
                        if let Some(suspend) = handle_surface(
                            &mut app,
                            &window,
                            &event,
                            &iad.instance,
                            &mut surface,
                            &renderer,
                            format,
                            &mut stored_surface_info,
                        ) {
                            suspended = suspend;
                        }

                        // We move to Wait when we get suspended so we don't spin at 50k FPS.
                        match event {
                            Event::Suspended => {
                                control_flow = winit::event_loop::ControlFlow::Wait;
                            }
                            Event::Resumed => {
                                control_flow = last_user_control_mode;
                            }
                            _ => {}
                        }

                        // We need to block all updates
                        if let Event::WindowEvent {
                            window_id: _,
                            event: winit::event::WindowEvent::RedrawRequested,
                        } = event
                        {
                            if suspended {
                                return;
                            }
                        }

                        app.handle_event(
                            &window,
                            &renderer,
                            &routines,
                            &base_rendergraph,
                            surface.as_ref(),
                            stored_surface_info.size,
                            event,
                            |c: winit::event_loop::ControlFlow| {
                                control_flow = c;
                                last_user_control_mode = c;
                            },
                            event_loop_window_target,
                        )
                    });
                }
            });
        }
    };
}
#[allow(clippy::too_many_arguments)]
fn handle_surface(
    app: &mut SceneViewer,
    window: &Window,
    event: &Event<()>,
    instance: &wgpu::Instance,
    surface: &mut Option<Arc<Surface>>,
    renderer: &Arc<Renderer>,
    format: rend3::types::TextureFormat,
    surface_info: &mut StoredSurfaceInfo,
) -> Option<bool> {
    match *event {
        Event::Resumed => {
            if surface.is_none() {
                *surface = Some(Arc::new(
                    unsafe { instance.create_surface(window) }.unwrap(),
                ));
            }
            Some(false)
        }
        Event::Suspended => {
            *surface = None;
            Some(true)
        }
        Event::WindowEvent {
            event: winit::event::WindowEvent::Resized(size),
            ..
        } => {
            log::debug!("resize {:?}", size);

            let size = UVec2::new(size.width, size.height);
            if let Some(ref mut inox_renderer) = app.inox_renderer {
                inox_renderer.resize(size)
            };
            if size.x == 0 || size.y == 0 {
                return Some(false);
            }

            surface_info.size = size;
            surface_info.scale_factor = app.scale_factor();
            surface_info.sample_count = app.sample_count();
            surface_info.present_mode = app.present_mode();

            // Winit erroniously stomps on the canvas CSS when a scale factor
            // change happens, so we need to put it back to normal. We can't
            // do this in a scale factor changed event, as the override happens
            // after the event is sent.
            //
            // https://github.com/rust-windowing/winit/issues/3023
            #[cfg(target_arch = "wasm32")]
            {
                use winit::platform::web::WindowExtWebSys;
                let canvas = window.canvas().unwrap();
                let style = canvas.style();

                style.set_property("width", "100%").unwrap();
                style.set_property("height", "100%").unwrap();
            }

            let inox_texture = renderer.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("inox texture"),
                size: Extent3d {
                    width: size.x,
                    height: size.y,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[wgpu::TextureFormat::Bgra8Unorm],
            });
            app.inox_texture = Some(inox_texture);
            // Reconfigure the surface for the new size.
            rend3::configure_surface(
                surface.as_ref().unwrap(),
                &renderer.device,
                TextureFormat::Bgra8Unorm,
                size,
                surface_info.present_mode,
            );
            let alpha_mode = wgpu::CompositeAlphaMode::Auto;
            let config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
                format: wgpu::TextureFormat::Bgra8Unorm,
                width: size.x,
                height: size.y,
                present_mode: wgpu::PresentMode::Immediate,
                alpha_mode,
                view_formats: Vec::new(),
            };
            surface
                .as_ref()
                .unwrap()
                .configure(&renderer.device, &config);
            // Tell the renderer about the new aspect ratio.
            renderer.set_aspect_ratio(size.x as f32 / size.y as f32);
            Some(false)
        }
        _ => None,
    }
}
#[cfg(not(target_arch = "wasm32"))]
fn winit_run<F, T>(
    event_loop: winit::event_loop::EventLoop<T>,
    event_handler: F,
) -> Result<(), winit::error::EventLoopError>
where
    F: FnMut(winit::event::Event<T>, &EventLoopWindowTarget<T>) + 'static,
    T: 'static,
{
    event_loop.run(event_handler)
}

#[cfg(target_arch = "wasm32")]
fn winit_run<F, T>(event_loop: EventLoop<T>, event_handler: F)
where
    F: FnMut(winit::event::Event<T>, &EventLoopWindowTarget<T>) + 'static,
    T: 'static,
{
    use wasm_bindgen::prelude::*;

    let winit_closure =
        Closure::once_into_js(move || event_loop.run(event_handler).expect("Init failed"));

    // make sure to handle JS exceptions thrown inside start.
    // Otherwise wasm_bindgen_futures Queue would break and never handle any tasks
    // again. This is required, because winit uses JS exception for control flow
    // to escape from `run`.
    if let Err(error) = call_catch(&winit_closure) {
        let is_control_flow_exception = error.dyn_ref::<js_sys::Error>().map_or(false, |e| {
            e.message().includes("Using exceptions for control flow", 0)
        });

        if !is_control_flow_exception {
            web_sys::console::error_1(&error);
        }
    }

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(catch, js_namespace = Function, js_name = "prototype.call.call")]
        fn call_catch(this: &JsValue) -> Result<(), JsValue>;
    }
}
