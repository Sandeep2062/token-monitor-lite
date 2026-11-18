use anyhow::Result;
use slint::{CloseRequestResponse, ComponentHandle, ModelRc, SharedPixelBuffer, VecModel};
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use token_monitor_lite::application::{
    ApplicationCommand, ApplicationError, ApplicationService, ApplicationSubmitError,
};
use token_monitor_lite::discovery::{HostPaths, resolve_paths};
use token_monitor_lite::pricing::{self, PricingCatalog};
use token_monitor_lite::tray;
use token_monitor_lite::ui::{
    ChoiceRow, HeatCell, HeatmapRow, ImportReportRow, ModelRow, MonthRow, SessionRow,
    SettingsClientRow, SettingsSourceRow, SourceHealthRow, SourceSeverity, TrendRow, UiController,
    UiPresentation,
};
use token_monitor_lite::update::{self, InstallOutcome, UpdateCheck, UpdatePlatform};

slint::include_modules!();

fn main() -> Result<()> {
    let paths = HostPaths::current();
    let update_root = paths
        .as_ref()
        .ok()
        .and_then(|paths| resolve_paths(paths).ok())
        .map(|resolved| resolved.native.root().to_path_buf());
    let application = paths
        .map_err(|_| StartupState::HostUnavailable)
        .and_then(|paths| ApplicationService::bootstrap(paths).map_err(StartupState::Application));
    run(application, update_root)
}

enum StartupState {
    HostUnavailable,
    Application(ApplicationError),
}

fn run(
    application: Result<ApplicationService, StartupState>,
    update_root: Option<PathBuf>,
) -> Result<()> {
    let window = AppWindow::new()?;
    let mut initial_controller = UiController::new();
    let (pricing_catalog, _) = PricingCatalog::load_or_bundled(update_root.as_deref());
    initial_controller.set_pricing_catalog(pricing_catalog);
    let application = match application {
        Ok(application) => {
            if let Ok(snapshot) = application.snapshot() {
                initial_controller.accept_application_snapshot(snapshot);
            }
            Some(application)
        }
        Err(state) => {
            initial_controller.set_startup_notice(startup_notice(&state));
            None
        }
    };
    // Create the tray before the first presentation so a tray that could not be
    // created is never advertised as a way back to a hidden window. It is only
    // constructed where it can actually be shown: on Linux the backend connects
    // to the session bus, and doing that without one holds up startup before the
    // window is ever mapped.
    let tray_capable = tray::tray_capable();
    let tray = if tray_capable {
        TrayIcon::new().ok().map(std::rc::Rc::new)
    } else {
        None
    };
    let tray_capable = tray.is_some();
    initial_controller.set_tray_available(tray_capable);
    let controller = Arc::new(Mutex::new(initial_controller));
    apply_presentation(&window, presentation(&controller));
    let tray_state = RefCell::new(None::<(String, String, String)>);
    if let Some(tray_handle) = tray.as_ref() {
        let tray = tray_handle.as_ref();
        // Without a status-notifier host the icon cannot be shown, so do not ask
        // the backend to register one: nothing may depend on an invisible icon.
        tray.set_shown(tray_capable);
        sync_tray(tray, &tray_state, &presentation(&controller));
        {
            let weak = window.as_weak();
            tray.on_open_window(move || {
                if let Some(window) = weak.upgrade() {
                    let _ = window.show();
                }
            });
        }
        {
            let weak = window.as_weak();
            tray.on_toggle_window(move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };
                if window.window().is_visible() {
                    let _ = window.hide();
                } else {
                    let _ = window.show();
                }
            });
        }
        {
            let application = application.clone();
            let weak = window.as_weak();
            let controller = controller.clone();
            tray.on_refresh_now(move || {
                if let Some(application) = application.as_ref()
                    && let Err(error) = application.submit(ApplicationCommand::Refresh)
                {
                    show_submit_error(&weak, &controller, error);
                }
            });
        }
        {
            let weak_tray = std::rc::Rc::downgrade(tray_handle);
            tray.on_quit_app(move || {
                if let Some(tray) = weak_tray.upgrade() {
                    tray.set_shown(false);
                }
                let _ = slint::quit_event_loop();
            });
        }
    }
    let pricing_handle = update_root.as_ref().map(|root| {
        let weak = window.as_weak();
        let controller = controller.clone();
        pricing::spawn(root.clone(), move |catalog| {
            let weak = weak.clone();
            let controller = controller.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Ok(mut controller) = controller.lock() {
                    controller.set_pricing_catalog(catalog);
                }
                if let Some(window) = weak.upgrade() {
                    apply_presentation(&window, presentation(&controller));
                }
            });
        })
    });

    // The limits lane. It is created only when a provider this build can probe
    // has a credential in the keyring, so an unconfigured install never starts a
    // thread, never schedules a probe, and never touches the network. Its worker
    // blocks on a condition variable until a probe is due; it does not poll.
    let limits_worker: Option<std::rc::Rc<token_monitor_lite::limits::scheduler::LimitsWorker>> =
        application.as_ref().and_then(|application| {
            use token_monitor_lite::limits::collector::CredentialTickets;
            let credentials = Arc::new(application.credentials());
            let now_ms = limits_now_ms();
            // One ticket allocator for the whole lane, seeded above the application's
            // own counter so a probe read can never look stale to a settings write.
            let tickets = CredentialTickets::new(1_000_000);
            let mut scheduler = token_monitor_lite::limits::scheduler::Scheduler::new(
                token_monitor_lite::limits::scheduler::INTERVAL_MS,
            );
            let mut enabled = 0_usize;
            for provider in token_monitor_lite::limits::bridge::PROVIDERS {
                if token_monitor_lite::limits::bridge::should_probe(
                    credentials.as_ref(),
                    provider,
                    tickets.next(),
                ) {
                    scheduler.enable(provider, now_ms);
                    enabled += 1;
                }
            }
            if enabled == 0 {
                return None;
            }
            let source =
                token_monitor_lite::limits::bridge::CollectorSource::new(credentials, tickets);
            let weak = window.as_weak();
            let controller = controller.clone();
            Some(
                token_monitor_lite::limits::scheduler::LimitsWorker::spawn(
                    scheduler,
                    Box::new(source),
                    Box::new(move |snapshot| {
                        let weak = weak.clone();
                        let controller = controller.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Ok(mut controller) = controller.lock() {
                                token_monitor_lite::limits::bridge::apply(
                                    &mut controller,
                                    &snapshot,
                                    limits_now_ms(),
                                );
                            }
                            if let Some(window) = weak.upgrade() {
                                apply_presentation(&window, presentation(&controller));
                            }
                        });
                    }),
                )
                .into(),
            )
        });

    let delivery_timer = slint::Timer::default();
    let tray_for_timer = tray.clone();
    if let Some(application) = application.clone() {
        let weak = window.as_weak();
        let controller = controller.clone();
        delivery_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(100),
            move || {
                let changed = application.snapshot().is_ok_and(|snapshot| {
                    let accepted = controller.lock().is_ok_and(|mut controller| {
                        controller.accept_application_snapshot(snapshot.clone())
                    });
                    // Quota rows arrive on the same application snapshot, so they
                    // are applied in the same pass: no extra timer, no extra wake.
                    let limits_changed = controller.lock().is_ok_and(|mut controller| {
                        token_monitor_lite::limits::bridge::apply(
                            &mut controller,
                            &snapshot.limits,
                            limits_now_ms(),
                        )
                    });
                    accepted || limits_changed
                });
                if changed && let Some(window) = weak.upgrade() {
                    let presentation = presentation(&controller);
                    apply_presentation(&window, presentation.clone());
                    if let Some(tray) = tray_for_timer.as_ref() {
                        sync_tray(tray.as_ref(), &tray_state, &presentation);
                    }
                }
            },
        );
    }

    {
        // The folder picker. `rfd` blocks on the dialog, and Slint's event loop is
        // the one thread that must not block, so the dialog runs on its own thread
        // and the result comes back through the event loop.
        let weak = window.as_weak();
        let controller = controller.clone();
        window.on_export_folder_pick(move || {
            let weak = weak.clone();
            let controller = controller.clone();
            std::thread::spawn(move || {
                let picked = rfd::FileDialog::new()
                    .set_title("Choose an export folder")
                    .pick_folder();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(window) = weak.upgrade() else {
                        return;
                    };
                    if let Some(folder) = picked {
                        if let Ok(mut controller) = controller.lock() {
                            controller.set_export_folder(&folder.to_string_lossy());
                            controller.set_export_notice(
                                "Folder chosen. Press Save to keep it, or Export now.",
                            );
                        }
                    } else {
                        if let Ok(mut controller) = controller.lock() {
                            controller.set_export_notice("No folder chosen.");
                        }
                    }
                    apply_presentation(&window, presentation(&controller));
                });
            });
        });
    }
    {
        // Turning auto-export on or off. The value goes through the draft, so the
        // existing revision CAS decides when it is actually persisted.
        let weak = window.as_weak();
        let controller = controller.clone();
        window.on_export_folder_changed(move |folder| {
            if let Ok(mut controller) = controller.lock() {
                controller.set_export_folder(&folder);
                controller.set_export_notice(if folder.is_empty() {
                    "Auto-export off. Press Save to keep this."
                } else {
                    "Auto-export on. Press Save to keep this."
                });
            }
            if let Some(window) = weak.upgrade() {
                apply_presentation(&window, presentation(&controller));
            }
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        let application = application.clone();
        window.on_export_now(move || {
            let Some(application) = application.as_ref() else {
                return;
            };
            // The committed folder is what an explicit export writes to; an
            // unsaved draft must not silently become the target.
            let folder = application.snapshot().ok().and_then(|snapshot| {
                snapshot
                    .settings
                    .export_folder
                    .clone()
                    .map(std::path::PathBuf::from)
            });
            match folder {
                Some(folder) => {
                    if let Ok(mut controller) = controller.lock() {
                        controller.set_export_notice("Exporting…");
                    }
                    if let Err(error) = application.submit(ApplicationCommand::ExportNow { folder })
                    {
                        show_submit_error(&weak, &controller, error);
                    }
                }
                None => {
                    if let Ok(mut controller) = controller.lock() {
                        controller
                            .set_export_notice("Choose a folder and press Save before exporting.");
                    }
                    if let Some(window) = weak.upgrade() {
                        apply_presentation(&window, presentation(&controller));
                    }
                }
            }
        });
    }

    // The auto-export tick. It runs on the same 60-second cadence as the
    // application's own delivery pass and asks for an attempt; the application
    // decides whether one is due and whether the payload changed, so an idle
    // machine neither probes nor rewrites a synced folder.
    let export_timer = slint::Timer::default();
    if let Some(application) = application.clone() {
        let application = application.clone();
        export_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(export_tick_seconds()),
            move || {
                let _ = application.submit(ApplicationCommand::AutoExport);
            },
        );
    }

    {
        // Appearance, currency and language choices. Each writes the draft only;
        // Save is what persists them, through the existing revision CAS.
        macro_rules! choice_handler {
            ($callback:ident, $setter:ident) => {{
                let weak = window.as_weak();
                let controller = controller.clone();
                window.$callback(move |value| {
                    if let Ok(mut controller) = controller.lock() {
                        controller.$setter(&value);
                    }
                    if let Some(window) = weak.upgrade() {
                        apply_presentation(&window, presentation(&controller));
                    }
                });
            }};
        }
        choice_handler!(on_theme_changed, set_theme);
        choice_handler!(on_density_changed, set_density);
        choice_handler!(on_motion_changed, set_motion);
        choice_handler!(on_currency_changed, set_currency);
        choice_handler!(on_language_changed, set_language);
    }

    {
        let application = application.clone();
        let weak_tray = tray.as_ref().map(std::rc::Rc::downgrade);
        // Held until the window closes: dropping the worker stops its thread.
        let limits_worker = limits_worker.clone();
        window.window().on_close_requested(move || {
            let _holds_limits_worker = &limits_worker;
            let hides_to_tray = tray_capable
                && application.as_ref().is_some_and(|application| {
                    application
                        .snapshot()
                        .is_ok_and(|snapshot| snapshot.settings.close_to_tray)
                });
            if hides_to_tray {
                return CloseRequestResponse::HideWindow;
            }
            if let Some(tray) = weak_tray.as_ref().and_then(std::rc::Weak::upgrade) {
                tray.set_shown(false);
            }
            let _ = slint::quit_event_loop();
            CloseRequestResponse::HideWindow
        });
    }

    {
        let weak = window.as_weak();
        window.on_navigate(move |view| {
            if let Some(window) = weak.upgrade() {
                window.set_active_view(view);
            }
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        window.on_search_changed(move |query| {
            if let Ok(mut controller) = controller.lock() {
                controller.set_search(&query);
            }
            if let Some(window) = weak.upgrade() {
                apply_presentation(&window, presentation(&controller));
            }
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        window.on_period_changed(move |index| {
            if let Ok(mut controller) = controller.lock() {
                controller.set_period_index(usize::try_from(index).unwrap_or(0));
            }
            if let Some(window) = weak.upgrade() {
                apply_presentation(&window, presentation(&controller));
            }
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        window.on_previous_page(move || {
            if let Ok(mut controller) = controller.lock() {
                controller.previous_page();
            }
            if let Some(window) = weak.upgrade() {
                apply_presentation(&window, presentation(&controller));
            }
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        window.on_next_page(move || {
            if let Ok(mut controller) = controller.lock() {
                controller.next_page();
            }
            if let Some(window) = weak.upgrade() {
                apply_presentation(&window, presentation(&controller));
            }
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        window.on_select_session(move |key| {
            let selected = controller
                .lock()
                .is_ok_and(|mut controller| controller.select_session(&key));
            if selected && let Some(window) = weak.upgrade() {
                window.set_active_view(1);
                apply_presentation(&window, presentation(&controller));
            }
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        window.on_clear_session_selection(move || {
            if let Ok(mut controller) = controller.lock() {
                controller.clear_session_selection();
            }
            if let Some(window) = weak.upgrade() {
                apply_presentation(&window, presentation(&controller));
            }
        });
    }
    let update_available = Arc::new(Mutex::new(None::<UpdateCheck>));
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        let available = update_available.clone();
        window.on_check_updates(move || {
            begin_update_check(weak.clone(), controller.clone(), available.clone());
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        let available = update_available.clone();
        let root = update_root.clone();
        window.on_install_update(move || {
            begin_update_install(
                weak.clone(),
                controller.clone(),
                available.clone(),
                root.clone(),
            );
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        let available = update_available.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            let _ = slint::invoke_from_event_loop(move || {
                begin_update_check(weak, controller, available);
            });
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        let application = application.clone();
        let limits = limits_worker.clone();
        window.on_refresh(move || {
            if let Some(application) = application.as_ref()
                && let Err(error) = application.submit(ApplicationCommand::Refresh)
            {
                show_submit_error(&weak, &controller, error);
            }
            // A person pressing Refresh is an explicit probe reason. It is not a
            // reason a hover or a focus can produce.
            if let Some(limits) = limits.as_ref() {
                for provider in token_monitor_lite::limits::bridge::PROVIDERS {
                    limits.request(provider, token_monitor_lite::limits::Trigger::Manual);
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        window.on_settings_timezone_changed(move |timezone| {
            if let Ok(mut controller) = controller.lock() {
                controller.set_settings_timezone(&timezone);
            }
            if let Some(window) = weak.upgrade() {
                apply_presentation(&window, presentation(&controller));
            }
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        window.on_settings_close_to_tray_changed(move |close_to_tray| {
            if let Ok(mut controller) = controller.lock() {
                controller.set_settings_close_to_tray(close_to_tray);
            }
            if let Some(window) = weak.upgrade() {
                apply_presentation(&window, presentation(&controller));
            }
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        window.on_settings_client_changed(move |client, enabled| {
            if let Ok(mut controller) = controller.lock() {
                controller.set_settings_client_enabled(&client, enabled);
            }
            if let Some(window) = weak.upgrade() {
                apply_presentation(&window, presentation(&controller));
            }
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        window.on_settings_source_changed(move |source, selected| {
            if let Ok(mut controller) = controller.lock() {
                controller.set_settings_source_selected(&source, selected);
            }
            if let Some(window) = weak.upgrade() {
                apply_presentation(&window, presentation(&controller));
            }
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        window.on_select_all_sources(move || {
            if let Ok(mut controller) = controller.lock() {
                controller.select_all_settings_sources();
            }
            if let Some(window) = weak.upgrade() {
                apply_presentation(&window, presentation(&controller));
            }
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        let application = application.clone();
        window.on_save_settings(move || {
            let draft = controller
                .lock()
                .ok()
                .and_then(|controller| controller.settings_save_draft());
            if let (Some(application), Some(draft)) = (application.as_ref(), draft)
                && let Err(error) = application.submit(ApplicationCommand::SaveSettings(draft))
            {
                show_submit_error(&weak, &controller, error);
            }
        });
    }
    {
        let weak = window.as_weak();
        let controller = controller.clone();
        let application = application.clone();
        window.on_cancel_settings(move || {
            let revision = controller.lock().ok().and_then(|mut controller| {
                let revision = controller.settings_draft_base_revision();
                controller.cancel_settings_draft();
                revision
            });
            if let Some(window) = weak.upgrade() {
                apply_presentation(&window, presentation(&controller));
            }
            if let (Some(application), Some(base_revision)) = (application.as_ref(), revision)
                && let Err(error) =
                    application.submit(ApplicationCommand::CancelSettings { base_revision })
            {
                show_submit_error(&weak, &controller, error);
            }
        });
    }

    // `ComponentHandle::run` used to show the window as part of starting the
    // loop. `run_event_loop_until_quit` does not: it runs until something asks it
    // to stop, which is what keeps a hidden-to-tray window alive. The window has
    // to be mapped explicitly.
    window.show().map_err(anyhow::Error::from)?;
    let event_loop_result = slint::run_event_loop_until_quit();
    if let Some(mut handle) = pricing_handle {
        handle.stop();
    }
    delivery_timer.stop();
    drop(delivery_timer);
    drop(window);
    let shutdown_result = application.map_or(Ok(()), ApplicationService::shutdown);
    event_loop_result.map_err(anyhow::Error::from)?;
    shutdown_result.map_err(anyhow::Error::from)
}

fn startup_notice(state: &StartupState) -> &'static str {
    match state {
        StartupState::HostUnavailable => {
            "Host folders are unavailable. Configuration cannot start."
        }
        StartupState::Application(ApplicationError::Migration) => {
            "Migration requires attention. Legacy originals were not changed."
        }
        StartupState::Application(ApplicationError::StorageOpen) => {
            "Settings recovery is required. Monitoring did not start."
        }
        StartupState::Application(ApplicationError::InvalidSettings) => {
            "Saved settings are unavailable. Monitoring did not start."
        }
        StartupState::Application(_) => {
            "Application services are unavailable. Monitoring did not start."
        }
    }
}

fn show_submit_error(
    weak: &slint::Weak<AppWindow>,
    controller: &Mutex<UiController>,
    error: ApplicationSubmitError,
) {
    if let Ok(mut controller) = controller.lock() {
        controller.set_command_notice(match error {
            ApplicationSubmitError::Busy => "The application is busy. Try again shortly.",
            ApplicationSubmitError::Closed => "The application is shutting down.",
        });
    }
    if let Some(window) = weak.upgrade() {
        apply_presentation(&window, presentation(controller));
    }
}

/// How often the UI asks the application whether an auto-export is due.
///
/// One tick per minute matches the default cadence; the application owns the
/// actual interval and the does-it-change check, so this is only how often it is
/// asked. A tick that is not due costs a mutex and a comparison.
fn export_tick_seconds() -> u64 {
    token_monitor_lite::export::schedule::DEFAULT_INTERVAL_SECONDS
}

/// Wall clock for the limits lane.
fn limits_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn presentation(controller: &Mutex<UiController>) -> UiPresentation {
    controller.lock().map_or_else(
        |_| UiController::new().presentation(),
        |value| value.presentation(),
    )
}

/// Push the palette, density and motion into the `Theme` global, and select the
/// interface language.
///
/// The palette lives in `src/theme.rs` so its contrast is testable; this is only
/// the transfer. Language selection is idempotent — Slint re-reads the catalogue
/// on each call, and re-selecting the same language is a no-op — so calling it on
/// every presentation is cheaper than tracking the last value.
fn apply_appearance(window: &AppWindow, presentation: &UiPresentation) {
    let dark = presentation.appearance.dark;
    let palette = token_monitor_lite::theme::Palette::for_dark(dark);
    let theme = window.global::<Theme>();
    let colour = |value: token_monitor_lite::theme::Rgb| {
        slint::Color::from_rgb_u8(value.r, value.g, value.b)
    };
    theme.set_background(colour(palette.background));
    theme.set_surface(colour(palette.surface));
    theme.set_surface_alt(colour(palette.surface_alt));
    theme.set_text(colour(palette.text));
    theme.set_text_muted(colour(palette.text_muted));
    theme.set_text_faint(colour(palette.text_faint));
    theme.set_border(colour(palette.border));
    theme.set_accent(colour(palette.accent));
    theme.set_warning(colour(palette.warning));
    theme.set_danger(colour(palette.danger));
    theme.set_success(colour(palette.success));

    let (row_height, row_gap) = presentation.appearance.density.metrics();
    // Plain integers: the generated setter takes an `int` and the Slint side adds
    // the unit. Spacing visibly changes, which is what the setting promises.
    theme.set_row_height(row_height);
    theme.set_row_gap(row_gap);
    theme.set_reduced_motion(presentation.appearance.reduced_motion);

    // English is the source language, so selecting it (or failing to find a
    // catalogue) leaves the built-in strings in place rather than blanking them.
    let language = presentation.appearance.language;
    if language != token_monitor_lite::settings::Language::En {
        let _ = slint::select_bundled_translation(language.catalogue());
    } else {
        let _ = slint::select_bundled_translation("en");
    }
}

fn apply_presentation(window: &AppWindow, presentation: UiPresentation) {
    apply_appearance(window, &presentation);
    window.set_tracked_tokens(presentation.home.tracked_tokens.into());
    window.set_input_tokens(presentation.home.input.into());
    window.set_output_tokens(presentation.home.output.into());
    window.set_cache_tokens(presentation.home.cache.into());
    window.set_reasoning_tokens(presentation.home.reasoning.into());
    window.set_cost(presentation.home.cost.into());
    window.set_empty_message(presentation.home.empty_message.into());
    window.set_source_headline(presentation.home.source_headline.into());
    window.set_period_heading(presentation.home.period_heading.into());
    window.set_period_index(presentation.home.period_index);
    window.set_has_usage(presentation.home.has_usage);
    window.set_input_permille(presentation.home.input_permille);
    window.set_output_permille(presentation.home.output_permille);
    window.set_cache_permille(presentation.home.cache_permille);
    window.set_reasoning_permille(presentation.home.reasoning_permille);
    window.set_page_label(presentation.page_label.into());
    window.set_trends_headline(presentation.trends.headline.into());
    window.set_trends(trend_model(presentation.trends.rows));
    window.set_trend_months(month_model(presentation.trends.months));
    window.set_trend_heatmap(heatmap_model(presentation.trends.heatmap));
    window.set_heatmap_note(presentation.trends.heatmap_note.into());
    window.set_models_headline(presentation.models.headline.into());
    window.set_models(model_model(presentation.models.rows));
    let detail = presentation.detail;
    window.set_detail_visible(detail.is_some());
    if let Some(detail) = detail {
        window.set_detail_heading(detail.heading.into());
        window.set_detail_model(detail.selected_model.into());
        window.set_detail_tracked(detail.tracked.into());
        window.set_detail_input(detail.input.into());
        window.set_detail_output(detail.output.into());
        window.set_detail_cache(detail.cache.into());
        window.set_detail_reasoning(detail.reasoning.into());
        window.set_detail_first_seen(detail.first_seen.into());
        window.set_detail_last_seen(detail.last_seen.into());
        window.set_detail_models(model_model(detail.models));
    } else {
        window.set_detail_heading("".into());
        window.set_detail_model("".into());
        window.set_detail_tracked("".into());
        window.set_detail_input("".into());
        window.set_detail_output("".into());
        window.set_detail_cache("".into());
        window.set_detail_reasoning("".into());
        window.set_detail_first_seen("".into());
        window.set_detail_last_seen("".into());
        window.set_detail_models(model_model(Vec::new()));
    }
    window.set_selection_notice(presentation.selection_notice.unwrap_or_default().into());
    window.set_refresh_available(presentation.refresh_available);
    window.set_can_previous(presentation.can_previous);
    window.set_can_next(presentation.can_next);
    window.set_watch_status(presentation.watch_status.into());
    window.set_update_status(presentation.update.status.into());
    window.set_update_available(presentation.update.available);
    window.set_update_busy(presentation.update.busy);
    window.set_recent_sessions(session_model(presentation.recent_sessions));
    window.set_sessions(session_model(presentation.sessions));
    window.set_sources(source_model(presentation.sources));
    window.set_settings_committed_timezone(presentation.settings.committed_timezone.into());
    window.set_settings_timezone(presentation.settings.draft_timezone.into());
    window.set_settings_validation_notice(presentation.settings.validation_notice.into());
    window.set_settings_notice(presentation.settings.notice.into());
    window.set_credential_status(presentation.settings.credential_status.into());
    window.set_settings_save_enabled(presentation.settings.save_enabled);
    window.set_settings_cancel_enabled(presentation.settings.cancel_enabled);
    window.set_settings_clients(settings_client_model(presentation.settings.clients));
    window.set_settings_sources(settings_source_model(presentation.settings.sources));
    window.set_import_report(import_report_model(presentation.settings.import_rows));
    let quota = presentation.quota;
    window.set_quota_headline(quota.headline.into());
    window.set_quota_rows(quota_model(quota.rows));
    window.set_quota_notice(quota.notice.unwrap_or_default().into());
    window.set_settings_committed_close_to_tray(presentation.settings.committed_close_to_tray);
    window.set_settings_close_to_tray(presentation.settings.draft_close_to_tray);
    window.set_tray_capable(presentation.settings.tray_capable);
    window.set_export_folder(presentation.settings.export_folder.into());
    window.set_export_notice(presentation.settings.export_notice.into());
    window.set_export_enabled(presentation.settings.export_enabled);
    window.set_theme_options(choice_model(presentation.settings.theme_options));
    window.set_density_options(choice_model(presentation.settings.density_options));
    window.set_motion_options(choice_model(presentation.settings.motion_options));
    window.set_currency_options(choice_model(presentation.settings.currency_options));
    window.set_language_options(choice_model(presentation.settings.language_options));
    window.set_appearance_summary(presentation.settings.appearance_summary.into());
    window.set_currency_summary(presentation.settings.currency_summary.into());
    window.set_language_summary(presentation.settings.language_summary.into());
    window.set_settings_warnings(presentation.settings.settings_warnings.into());
}

/// Render the badge bitmap Slint hands to the platform tray backend.
fn badge_image(tokens: &str, cost: &str) -> slint::Image {
    let pixels = tray::badge_rgba(tokens, cost);
    slint::Image::from_rgba8(SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        &pixels,
        tray::BADGE_SIZE,
        tray::BADGE_SIZE,
    ))
}

/// Push today's labels to the tray, and only when they actually changed.
///
/// The caller is the existing 100 ms snapshot delivery, so the tray adds no
/// timer, no watcher and no work while nothing is happening.
fn sync_tray(
    tray: &TrayIcon,
    state: &RefCell<Option<(String, String, String)>>,
    presentation: &UiPresentation,
) {
    let next = (
        presentation.tray.tokens.clone(),
        presentation.tray.cost.clone(),
        presentation.tray.tooltip.clone(),
    );
    if state.borrow().as_ref() == Some(&next) {
        return;
    }
    tray.set_badge(badge_image(&next.0, &next.1));
    tray.set_hover_text(next.2.clone().into());
    tray.set_label_text(format!("{} · {}", next.0, next.1).into());
    *state.borrow_mut() = Some(next);
}

fn begin_update_check(
    weak: slint::Weak<AppWindow>,
    controller: Arc<Mutex<UiController>>,
    available: Arc<Mutex<Option<UpdateCheck>>>,
) {
    if let Ok(mut controller) = controller.lock() {
        controller.set_update_checking();
    }
    if let Some(window) = weak.upgrade() {
        apply_presentation(&window, presentation(&controller));
    }
    std::thread::spawn(move || {
        let result = update::check_for_update(
            update::CURRENT_VERSION,
            UpdatePlatform::current(),
            &update::HttpsTransport,
        );
        let weak_ui = weak.clone();
        let controller_ui = controller.clone();
        let available_ui = available.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Ok(mut controller) = controller_ui.lock() {
                match &result {
                    Ok(Some(check)) => controller.set_update_available(&check.version),
                    Ok(None) => controller.set_update_current(update::CURRENT_VERSION),
                    Err(_) => controller.set_update_unavailable(),
                }
            }
            if let Ok(mut slot) = available_ui.lock() {
                *slot = result.ok().flatten();
            }
            if let Some(window) = weak_ui.upgrade() {
                apply_presentation(&window, presentation(&controller_ui));
            }
        });
    });
}

fn begin_update_install(
    weak: slint::Weak<AppWindow>,
    controller: Arc<Mutex<UiController>>,
    available: Arc<Mutex<Option<UpdateCheck>>>,
    root: Option<PathBuf>,
) {
    let Some(check) = available.lock().ok().and_then(|slot| slot.clone()) else {
        return;
    };
    if let Ok(mut controller) = controller.lock() {
        controller.set_update_downloading();
    }
    if let Some(window) = weak.upgrade() {
        apply_presentation(&window, presentation(&controller));
    }
    std::thread::spawn(move || {
        let root = root.unwrap_or_else(|| std::env::temp_dir().join("token-monitor-lite-updates"));
        let result =
            update::download_update(&check, &root, &update::HttpsTransport).and_then(|path| {
                update::install_update(&path, UpdatePlatform::current())
                    .map(|outcome| (path, outcome))
            });
        let quit = matches!(&result, Ok((_, InstallOutcome::Launched)));
        let weak_ui = weak.clone();
        let controller_ui = controller.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Ok(mut controller) = controller_ui.lock() {
                match &result {
                    Ok((path, InstallOutcome::Manual(_))) => {
                        controller.set_update_manual(&path.to_string_lossy());
                    }
                    Ok((_, InstallOutcome::Launched)) => controller.set_update_installing(),
                    Err(_) => controller.set_update_install_failed(),
                }
            }
            if let Some(window) = weak_ui.upgrade() {
                apply_presentation(&window, presentation(&controller_ui));
            }
            if quit {
                let _ = slint::quit_event_loop();
            }
        });
    });
}

fn session_model(rows: Vec<SessionRow>) -> ModelRc<SessionRowData> {
    ModelRc::new(VecModel::from(
        rows.into_iter()
            .map(|row| SessionRowData {
                selection_key: row.selection_key.into(),
                client: row.identity.client.into(),
                session: row.identity.session.into(),
                model: row.identity.model.into(),
                last_activity: row.last_activity.into(),
                input: row.input.into(),
                output: row.output.into(),
                cache: row.cache.into(),
                reasoning: row.reasoning.into(),
                tracked_tokens: row.tracked_tokens.into(),
                cost: row.cost.into(),
            })
            .collect::<Vec<_>>(),
    ))
}

fn trend_model(rows: Vec<TrendRow>) -> ModelRc<TrendRowData> {
    ModelRc::new(VecModel::from(
        rows.into_iter()
            .map(|row| TrendRowData {
                label: row.label.into(),
                total: row.total.into(),
                permille: row.permille,
            })
            .collect::<Vec<_>>(),
    ))
}

fn choice_model(rows: Vec<ChoiceRow>) -> ModelRc<ChoiceRowData> {
    ModelRc::new(VecModel::from(
        rows.into_iter()
            .map(|row| ChoiceRowData {
                value: row.value.into(),
                label: row.label.into(),
                selected: row.selected,
            })
            .collect::<Vec<_>>(),
    ))
}

fn month_model(rows: Vec<MonthRow>) -> ModelRc<MonthRowData> {
    ModelRc::new(VecModel::from(
        rows.into_iter()
            .map(|row| MonthRowData {
                month: row.month.into(),
                total: row.total.into(),
                permille: row.permille,
                partial: row.partial,
            })
            .collect::<Vec<_>>(),
    ))
}

fn heatmap_model(rows: Vec<HeatmapRow>) -> ModelRc<HeatmapRowData> {
    ModelRc::new(VecModel::from(
        rows.into_iter()
            .map(|row| HeatmapRowData {
                weekday: row.weekday.into(),
                cells: ModelRc::new(VecModel::from(
                    row.cells
                        .into_iter()
                        .map(|cell| HeatmapCellData {
                            level: match cell.level {
                                HeatCell::Unknown => 0,
                                HeatCell::Empty => 1,
                                HeatCell::Level1 => 2,
                                HeatCell::Level2 => 3,
                                HeatCell::Level3 => 4,
                                HeatCell::Level4 => 5,
                            },
                            tooltip: cell.tooltip.into(),
                        })
                        .collect::<Vec<_>>(),
                )),
            })
            .collect::<Vec<_>>(),
    ))
}

fn model_model(rows: Vec<ModelRow>) -> ModelRc<ModelRowData> {
    ModelRc::new(VecModel::from(
        rows.into_iter()
            .map(|row| ModelRowData {
                label: row.label.into(),
                total: row.total.into(),
                cost: row.cost.into(),
                permille: row.permille,
            })
            .collect::<Vec<_>>(),
    ))
}

fn quota_model(rows: Vec<token_monitor_lite::ui::QuotaRow>) -> ModelRc<QuotaRowData> {
    ModelRc::new(VecModel::from(
        rows.into_iter()
            .map(|row| QuotaRowData {
                provider: row.provider.into(),
                heading: row.heading.into(),
                windows: ModelRc::new(VecModel::from(
                    row.windows
                        .into_iter()
                        .map(|window| QuotaWindowData {
                            label: window.label.into(),
                            used_percent: window.used_percent,
                            amount: window.amount.into(),
                            reset: window.reset.into(),
                        })
                        .collect::<Vec<_>>(),
                )),
                status: row.status.into(),
                stale: row.stale,
                attention: row.attention,
            })
            .collect::<Vec<_>>(),
    ))
}

fn source_model(rows: Vec<SourceHealthRow>) -> ModelRc<SourceHealthRowData> {
    ModelRc::new(VecModel::from(
        rows.into_iter()
            .map(|row| SourceHealthRowData {
                name: row.name.into(),
                format: row.format.into(),
                status: row.status.into(),
                detail: row.detail.into(),
                severity: severity_value(row.severity),
            })
            .collect::<Vec<_>>(),
    ))
}

fn settings_client_model(rows: Vec<SettingsClientRow>) -> ModelRc<SettingsClientRowData> {
    ModelRc::new(VecModel::from(
        rows.into_iter()
            .map(|row| SettingsClientRowData {
                id: row.id.into(),
                label: row.label.into(),
                enabled: row.enabled,
                committed: row.committed,
            })
            .collect::<Vec<_>>(),
    ))
}

fn settings_source_model(rows: Vec<SettingsSourceRow>) -> ModelRc<SettingsSourceRowData> {
    ModelRc::new(VecModel::from(
        rows.into_iter()
            .map(|row| SettingsSourceRowData {
                id: row.id.into(),
                name: row.name.into(),
                client: row.client.into(),
                selected: row.selected,
                committed: row.committed,
                status: row.status.into(),
            })
            .collect::<Vec<_>>(),
    ))
}

fn import_report_model(rows: Vec<ImportReportRow>) -> ModelRc<ImportReportRowData> {
    ModelRc::new(VecModel::from(
        rows.into_iter()
            .map(|row| ImportReportRowData {
                artifact: row.artifact.into(),
                preservation: row.preservation.into(),
                semantic_import: row.semantic_import.into(),
                activation: row.activation.into(),
                records: row.records.into(),
            })
            .collect::<Vec<_>>(),
    ))
}

fn severity_value(severity: SourceSeverity) -> i32 {
    match severity {
        SourceSeverity::Healthy => 0,
        SourceSeverity::Waiting => 1,
        SourceSeverity::Retained => 2,
        SourceSeverity::WatcherUnavailable => 3,
        SourceSeverity::Failed => 4,
    }
}
