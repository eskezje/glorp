#![cfg_attr(feature = "packaged", windows_subsystem = "windows")]
use discord_rich_presence::{DiscordIpc, DiscordIpcClient, activity};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;
use std::{
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    sync::{Arc, Mutex, atomic::*},
};
use webview2_com::{Microsoft::Web::WebView2::Win32::*, *};
use windows::Win32::System::Threading::{
    GetCurrentProcess, PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling,
    SetProcessInformation,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::{
    Win32::{Foundation::*, System::Com::*, UI::WindowsAndMessaging::*},
    core::*,
};

type EventRegistrationToken = i64;

mod config;
mod constants;

mod utils;
mod window;
mod modules {
    pub mod blocklist;
    pub mod flaglist;
    pub mod inject;
    pub mod lifecycle;
    pub mod mmcss;
    pub mod priority;
    pub mod swapper;
    pub mod userscripts;
}

static LAUNCH_ARGS: Lazy<Arc<Mutex<Vec<String>>>> =
    Lazy::new(|| Arc::new(Mutex::new(std::env::args().skip(1).collect())));

static LAST_CONNECTED_LOBBY: Lazy<Arc<Mutex<IpAddr>>> =
    Lazy::new(|| Arc::new(Mutex::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)))));

static PING: Lazy<AtomicU32> = Lazy::new(|| AtomicU32::new(0));

struct StartupSettings {
    hard_flip: bool,
    uncap_fps: bool,
    discord_rpc: bool,
    mmcss: bool,
    blocklist: bool,
    swapper: bool,
    userscripts: bool,
    real_ping: bool,
    ramp_boost: bool,
    start_mode: String,
    webview_priority: String,
    #[cfg(feature = "packaged")]
    check_updates: bool,
}

fn main() {
    #[cfg(feature = "packaged")]
    {
        modules::lifecycle::set_panic_hook();
        modules::lifecycle::installer_cleanup().ok();
    }
    modules::lifecycle::register_instance();

    // Disable power throttling for best performance
    unsafe {
        let throttling_state = PROCESS_POWER_THROTTLING_STATE {
            Version: 1,
            ControlMask: 0x1, // PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            StateMask: 0,     // 0 = disable throttling
        };
        
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &throttling_state as *const _ as *const _,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        ).ok();
    }

    let user_profile = std::env::var("USERPROFILE").unwrap();
    let client_dir = PathBuf::from(&user_profile).join("Documents").join("glorp");
    let scripts_dir = client_dir.join("scripts");
    let flaglist_path = client_dir.join("flags.json");
    let blocklist_path = client_dir.join("blocklist.json");

    std::fs::create_dir_all(&client_dir).ok();
    std::fs::create_dir_all(&scripts_dir).ok();

    if !blocklist_path.exists() {
        std::fs::write(&blocklist_path, constants::DEFAULT_BLOCKLIST).ok();
    }
    if !flaglist_path.exists() {
        std::fs::write(&flaglist_path, constants::DEFAULT_FLAGS).ok();
    }

    let webview2_folder: PathBuf = std::env::current_dir().unwrap().join("WebView2");

    let config = Arc::new(Mutex::new(config::Config::load()));
    let startup = {
        let cfg = config.lock().unwrap();
        StartupSettings {
            hard_flip: cfg.get("hardFlip").unwrap_or(true),
            uncap_fps: cfg.get("uncapFps").unwrap_or(true),
            discord_rpc: cfg.get("discordRPC").unwrap_or(false),
            mmcss: cfg.get("mmcss").unwrap_or(true),
            blocklist: cfg.get("blocklist").unwrap_or(true),
            swapper: cfg.get("swapper").unwrap_or(true),
            userscripts: cfg.get("userscripts").unwrap_or(false),
            real_ping: cfg.get("realPing").unwrap_or(false),
            ramp_boost: cfg.get("rampBoost").unwrap_or(false),
            start_mode: cfg
                .get::<String>("startMode")
                .unwrap_or_else(|| String::from("Borderless Fullscreen")),
            webview_priority: cfg
                .get::<String>("webviewPriority")
                .unwrap_or_else(|| String::from("Normal")),
            #[cfg(feature = "packaged")]
            check_updates: cfg.get("checkUpdates").unwrap_or(false),
        }
    };

    if startup.hard_flip {
        std::fs::rename(
            webview2_folder.join("OLD_vk_swiftshader.dll"),
            &webview2_folder.join("vk_swiftshader.dll"),
        )
        .ok();
    } else {
        std::fs::rename(
            webview2_folder.join("vk_swiftshader.dll"),
            &webview2_folder.join("OLD_vk_swiftshader.dll"),
        )
        .ok();
    }

    let mut args = modules::flaglist::load();
    let discord_client: Mutex<Option<DiscordIpcClient>> = Mutex::new(None);

    if startup.uncap_fps {
        args.push_str(" --disable-frame-rate-limit")
    }
    
    // Add rendering optimization flags for best performance
    args.push_str(" --use-angle=d3d11"); // Force D3D11 backend
    args.push_str(" --disable-gpu-vsync"); // Allow tearing/VRR properly
    args.push_str(" --disable-features=CalculateNativeWinOcclusion"); // Prevent background throttling

    if startup.discord_rpc {
        let mut client = DiscordIpcClient::new(constants::DISCORD_CLIENT_ID);
        client.connect().ok();
        *discord_client.lock().unwrap() = Some(client);
    }

    unsafe {
        let (mut main_window, env) = window::Window::new(startup.start_mode.as_str(), args);

        modules::priority::set(startup.webview_priority.as_str());

        modules::priority::set(
            config
                .lock()
                .unwrap()
                .get::<String>("webviewPriority")
                .unwrap_or(String::from("Normal"))
                .as_str(),
        );

        let mut webview_pid: u32 = 0;
        main_window.webview.BrowserProcessId(&mut webview_pid).ok();

        let exe_dir = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let hook_dll = exe_dir.join("glorp_renderhook.dll"); // produced by the [lib] target we added

        // (Optional) make sure DLL exists (build step must have produced it)
        if std::fs::metadata(&hook_dll).is_ok() {
            if let Err(e) = modules::inject::inject_into_child_gpu_process(
                webview_pid,
                hook_dll.to_str().unwrap(),
            ) {
                eprintln!("DLL inject failed: {e:?}");
            } else {
                println!("Injected hook into GPU process.");
            }
        } else {
            eprintln!("Hook DLL not found at {:?}", hook_dll);
        }

        println!("Webview PID: {}", webview_pid);
        
        // Apply power throttling disable to webview process
        if let Err(e) = modules::mmcss::disable_process_power_throttling(webview_pid) {
            eprintln!("Failed to disable webview power throttling: {}", e);
        }
        
        // Enable DWM MMCSS scheduling (system-wide optimization)
        if let Err(e) = modules::mmcss::enable_dwm_mmcss() {
            eprintln!("Failed to enable DWM MMCSS: {}", e);
        }
        
        // Apply MMCSS to webview process if enabled
        if startup.mmcss {
            if let Err(e) = modules::mmcss::register_webview_process(webview_pid, "Games") {
                eprintln!("Failed to register MMCSS: {}", e);
            } else {
                println!("MMCSS enabled for Games task class");
            }
        }
        
        #[cfg(feature = "packaged")]
        {
            if startup.check_updates {
                modules::lifecycle::check_update();
            }
        }

        if startup.userscripts {
            if let Err(e) = modules::userscripts::load(&main_window.webview) {
                eprintln!("Failed to load userscripts: {}", e);
            }
        }

        #[cfg(feature = "editor-ignore")]
        {
            main_window
                .webview
                .AddScriptToExecuteOnDocumentCreated(
                    PCWSTR(utils::create_utf_string(include_str!("../target/bundle.js")).as_ptr()),
                    None,
                )
                .ok();
        }

        main_window.webview.Navigate(w!("https://krunker.io")).ok();

        // auto accept permission requests
        let mut permission_requested_token = EventRegistrationToken::default();
        main_window
            .webview
            .add_PermissionRequested(
                &PermissionRequestedEventHandler::create(Box::new(
                    move |_, args: Option<ICoreWebView2PermissionRequestedEventArgs>| {
                        if let Some(args) = args {
                            args.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW).ok();
                        }
                        Ok(())
                    },
                )),
                &mut permission_requested_token,
            )
            .ok();

        let mut blocklist: Vec<Regex> = Vec::new();
        let mut swaps: HashMap<String, IStream> = HashMap::new();

        if startup.blocklist {
            blocklist = modules::blocklist::load(&main_window.webview)
        };
        if startup.swapper {
            swaps = modules::swapper::load(&main_window.webview)
        };

        main_window
            .webview
            .AddWebResourceRequestedFilterWithRequestSourceKinds(
                PCWSTR(utils::create_utf_string("*://matchmaker.krunker.io/game-info*").as_ptr()),
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
            )
            .ok();

        let mut web_resource_requested_token = EventRegistrationToken::default();
        main_window.webview.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(
                move |webview: Option<ICoreWebView2>,
                      args: Option<ICoreWebView2WebResourceRequestedEventArgs>| {
                    if let Some(args) = args {
                        let request: ICoreWebView2WebResourceRequest = args.Request()?;
                        let mut uri_string = utils::create_utf_string("");
                        let uri = uri_string.as_mut_ptr() as *mut PWSTR;
                        request.Uri(uri)?;
                        let uri = uri.as_ref().unwrap().to_string()?;
                        let filename: &str = uri
                            .split("krunker.io/")
                            .nth(1)
                            .and_then(|s| s.split('?').next())
                            .unwrap_or("");

                        if filename.contains("game-info") || uri.contains("lobby-ranked") {
                            if let Some(webview) = webview {
                                webview.PostWebMessageAsString(w!("game-updated")).ok();
                            }
                            return Ok(());
                        }

                        let stream = swaps.get(filename);
                        if let Some(stream) = stream {
                            let response = env.CreateWebResourceResponse(
                                stream,
                                200,
                                w!("OK"),
                                w!("Access-Control-Allow-Origin: *"),
                                )?;
                            args.SetResponse(Some(&response))?;

                            return Ok(());
                        }

                        for regex in &blocklist {
                            if regex.is_match(&uri) {
                                request.SetUri(PCWSTR::null())?;
                                return Ok(());
                            }
                        }
                    }
                    Ok(())
                },
            )),
            &mut web_resource_requested_token,
        ).ok();

        let widget_wnd = Some(utils::find_child_window_by_class(
            FindWindowW(w!("krunker_webview"), PCWSTR::null()).unwrap(),
            "Chrome_RenderWidgetHostHWND",
        ));

        if startup.real_ping {
            main_window
                .webview
                .CallDevToolsProtocolMethod(w!("Network.enable"), w!("{}"), None)
                .ok();
            let ws_receiver = main_window
                .webview
                .GetDevToolsProtocolEventReceiver(w!("Network.webSocketCreated"))
                .unwrap();

            let handler =
                DevToolsProtocolEventReceivedEventHandler::create(Box::new(move |_, args| {
                    if let Some(args) = args {
                        let mut params_vec = utils::create_utf_string("");
                        let params = params_vec.as_mut_ptr() as *mut PWSTR;
                        args.ParameterObjectAsJson(params)?;
                        let json = serde_json::from_str::<serde_json::Value>(
                            &params.as_ref().unwrap().to_string().unwrap(),
                        )
                        .unwrap();
                        let url = json.get("url").unwrap().to_string();
                        if url.contains("lobby-") {
                            let host = url
                                .split("://")
                                .last()
                                .unwrap()
                                .split("/")
                                .next()
                                .unwrap()
                                .to_string();
                            let resolved_ips = dns_lookup::lookup_host(&host)?;
                            if let Some(ip) = resolved_ips.into_iter().next() {
                                *LAST_CONNECTED_LOBBY.lock().unwrap() = ip;
                            }
                        }
                    }
                    Ok(())
                }));

            let mut devtools_event_token = EventRegistrationToken::default();
            ws_receiver
                .add_DevToolsProtocolEventReceived(&handler, &mut devtools_event_token)
                .ok();

            std::thread::spawn(move || {
                loop {
                    let result = ping_rs::send_ping(
                        &LAST_CONNECTED_LOBBY.lock().unwrap(),
                        std::time::Duration::from_secs(1),
                        Default::default(),
                        Some(&ping_rs::PingOptions {
                            ttl: 128,
                            dont_fragment: true,
                        }),
                    );
                    if let Ok(reply) = result {
                        PING.store(reply.rtt, Ordering::Relaxed);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(3000));
                }
            });
        }

        let config_clone = Arc::clone(&config);

        fn set_cpu_throttling(
            webview: &ICoreWebView2,
            cfg: &Arc<Mutex<config::Config>>,
            key: &str,
            default: f32,
        ) {
            let rate = cfg.lock().unwrap().get::<f32>(key).unwrap_or(default);
            let payload = format!("{{\"rate\":{}}}", rate);
            let payload_utf16 = utils::create_utf_string(&payload);
            unsafe {
                webview
                    .CallDevToolsProtocolMethod(
                        w!("Emulation.setCPUThrottlingRate"),
                        PCWSTR(payload_utf16.as_ptr()),
                        None,
                    )
                    .ok();
            }
        }

set_cpu_throttling(&main_window.webview, &config_clone, "inMenuThrottle", 2.0);

        let mut web_message_received_token = EventRegistrationToken::default();

        main_window
            .webview
            .add_WebMessageReceived(
                &WebMessageReceivedEventHandler::create(Box::new(
                    move |webview, args: Option<ICoreWebView2WebMessageReceivedEventArgs>| {
                        let (Some(webview), Some(args)) = (webview, args) else {
                            return Ok(());
                        };
                        let mut message_vec = utils::create_utf_string("");
                        let message = message_vec.as_mut_ptr() as *mut PWSTR;
                        if args.TryGetWebMessageAsString(message).is_err() {
                            return Ok(());
                        }

                        let message_string = message.as_ref().unwrap().to_string().unwrap();

                        let parts: Vec<&str> =
                            message_string.split(", ").map(|s| s.trim()).collect();
                        match parts.as_slice() {
                            ["set-config", setting, value, ..] => {
                                let parsed_value = if let Ok(bool_val) = value.parse::<bool>() {
                                    serde_json::Value::Bool(bool_val)
                                } else if let Ok(int_val) = value.parse::<i64>() {
                                    serde_json::Value::Number(serde_json::Number::from(int_val))
                                } else if let Ok(float_val) = value.parse::<f64>() {
                                    let rounded = (float_val * 100.0).round() / 100.0;
                                    match serde_json::Number::from_f64(rounded) {
                                        Some(number) => serde_json::Value::Number(number),
                                        None => serde_json::Value::String((*value).to_string()),
                                    }
                                } else {
                                    serde_json::Value::String((*value).to_string())
                                };
                                if let Ok(mut cfg) = config_clone.lock() {
                                    cfg.set(setting, parsed_value);
                                }
                            }
                            ["get-info", ..] => {
                                let settings_value = config_clone
                                    .lock()
                                    .ok()
                                    .and_then(|cfg| serde_json::to_value(&*cfg).ok())
                                    .unwrap_or(serde_json::Value::Null);
                                let version = env!("CARGO_PKG_VERSION");
                                let mut info_map = serde_json::Map::new();
                                info_map.insert("settings".to_string(), settings_value);
                                info_map.insert(
                                    "version".to_string(),
                                    serde_json::Value::String(version.to_string()),
                                );
                                if let Ok(launch_args) = LAUNCH_ARGS.lock() {
                                    if !launch_args.is_empty() {
                                        info_map.insert(
                                            "launchArgs".to_string(),
                                            serde_json::Value::String(launch_args.join(" ")),
                                        );
                                    }
                                }

                                if let Ok(info_json) = serde_json::to_string_pretty(&info_map) {
                                    let info_utf16 = utils::create_utf_string(&info_json);
                                    webview
                                        .PostWebMessageAsJson(PCWSTR(info_utf16.as_ptr()))
                                        .ok();
                                }
                            }
                            ["pointer-lock", value, ..] => {
                                let enabled = value.parse::<bool>().unwrap_or(false);
                                PostMessageW(
                                    widget_wnd,
                                    WM_USER,
                                    WPARAM(enabled as usize),
                                    LPARAM(0),
                                )
                                .ok();
                                if enabled {
                                    set_cpu_throttling(&webview, &config_clone, "throttle", 1.0);
                                } else {
                                    set_cpu_throttling(
                                        &webview,
                                        &config_clone,
                                        "inMenuThrottle",
                                        2.0,
                                    );
                                }
                            }
                            ["close", ..] => {
                                PostQuitMessage(0);
                            }
                            ["open", target, ..] => {
                                if !target.is_empty() {
                                    let _ = std::process::Command::new("cmd")
                                        .args(["/C", "start", "", target])
                                        .spawn();
                                }
                            }
                            ["rpc-update", activity_state, map, ..] => {
                                let state = format!("{} on {}", activity_state, map);
                                if let Ok(mut client_guard) = discord_client.lock() {
                                    if let Some(client) = client_guard.as_mut() {
                                        let activity = activity::Activity::new()
                                            .details("Krunker")
                                            .state(&state)
                                            .assets(activity::Assets::new());

                                        if let Err(e) = client.set_activity(activity) {
                                            eprintln!("Failed to set rpc activity: {}", e);
                                        }
                                    }
                                }
                            }
                            ["ping", ..] => {
                                let ping_payload = utils::create_utf_string(&format!(
                                    "{{\"pingInfo\":{}}}",
                                    PING.load(Ordering::Relaxed)
                                ));
                                webview
                                    .PostWebMessageAsJson(PCWSTR(ping_payload.as_ptr()))                                    .ok();
                            }
                            _ => {}
                        }

                        Ok(())
                    },
                )),
                &mut web_message_received_token,
            )
            .ok();

        let mut accelerator_key_pressed_token = EventRegistrationToken::default();
        main_window
            .controller
            .clone()
            .add_AcceleratorKeyPressed(
                &AcceleratorKeyPressedEventHandler::create(Box::new(
                    move |_, args: Option<ICoreWebView2AcceleratorKeyPressedEventArgs>| {
                        let mut pressed_key: u32 = 0;
                        let mut key_event_kind: COREWEBVIEW2_KEY_EVENT_KIND =
                            COREWEBVIEW2_KEY_EVENT_KIND::default();
                        let Some(args) = args else {
                            return Ok(());
                        };

                        args.KeyEventKind(&mut key_event_kind)?;
                        args.VirtualKey(&mut pressed_key)?;
                        if key_event_kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN {
                            return Ok(());
                        }
                        match VIRTUAL_KEY(pressed_key as u16) {
                            VK_F4 | VK_F6 => {
                                main_window.webview.Navigate(w!("https://krunker.io")).ok();
                                PostMessageW(
                                    widget_wnd,
                                    WM_USER,
                                    WPARAM(false as usize),
                                    LPARAM(0),
                                )
                                .ok();
                            }
                            VK_F5 => {
                                main_window.webview.Reload().ok();
                                PostMessageW(
                                    widget_wnd,
                                    WM_USER,
                                    WPARAM(false as usize),
                                    LPARAM(0),
                                )
                                .ok();
                            }
                            VK_F11 => {
                                main_window.toggle_fullscreen();
                            }
                            VK_F12 => {
                                main_window.webview.OpenDevToolsWindow().ok();
                            }
                            _ => {}
                        }
                        Ok(())
                    },
                )),
                &mut accelerator_key_pressed_token,
            )
            .ok();

        if startup.ramp_boost {
            PostMessageW(widget_wnd, WM_APP, WPARAM(1), LPARAM(0)).ok();
        }

        let mut msg: MSG = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    };
    // code here runs after window is closed

    config.lock().unwrap().save();
}
