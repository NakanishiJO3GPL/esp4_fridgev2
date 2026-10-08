pub mod camera;
pub mod jpeg;
mod logger;
mod pthread_config;
pub mod wifi;

pub use camera::{thread as camera_thread, trigger};

use camera::capture;
use jpeg::encoder;
use wifi::service;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // It is necessary to call this function once. Otherwise, some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    logger::initialize().expect("failed to initialize ESP-IDF logging");

    let wifi_service = service::start(service::Config::from_build()?)?;
    let jpeg_publisher = wifi_service.publisher();
    let trigger_publisher = jpeg_publisher.clone();

    let config = capture::Config {
        xclk_gpio: None,
        reset_gpio: None,
        pwdn_gpio: None,

        ..capture::Config::new(7, 8)
    };
    // Sample three configured regions for APL-based trigger detection.
    let regions = [
        // Left side
        capture::Region {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        // Center
        capture::Region {
            x: 920,
            y: 1040,
            width: 80,
            height: 40,
        },
        // Right side
        capture::Region {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
    ];
    // Initial tuning values from the reference detector, in raw Y units.
    let mut apl_trigger = camera::trigger::AplTrigger::new(camera::trigger::AplTriggerConfig {
        settle_velocity_abs_max: 0.05,
        settle_frames_required: 2,
        wait_to_detect_velocity_abs_min: 0.2,
    });
    let jpeg_config = encoder::Config { quality: 60 };

    let (jpeg_sender, jpeg_thread) = encoder::spawn(jpeg_config, move |image| {
        if let Err(error) = jpeg_publisher.publish(image) {
            log::warn!("JPEG publication failed; previous image retained: {error}");
        }
    })?;
    let mut jpeg_available = true;

    let capture_thread = camera::thread::spawn(config, move |frame| {
        // Compute on every acquired frame for trigger detection.
        let means = frame.average_luminance(&regions);
        match &means {
            Ok(means) => {
                if let Some(notification) = apl_trigger.update_center(frame.sequence(), means) {
                    trigger_publisher.notify_trigger(notification.frame_sequence);
                    log::info!(
                        target: logger::CAPTURE_TRIGGER_TARGET,
                        "Capture trigger: frame {}, center APL={:.2}, delta={:.4}",
                        notification.frame_sequence,
                        notification.center_apl,
                        notification.velocity,
                    );
                    if jpeg_available {
                        match jpeg_sender.try_submit(frame, notification) {
                            Ok(true) => {}
                            Ok(false) => {
                                log::debug!(
                                    "JPEG busy; skipped trigger at frame {}",
                                    frame.sequence()
                                )
                            }
                            Err(error) => {
                                jpeg_available = false;
                                log::error!("JPEG requests disabled; capture continues: {error}");
                            }
                        }
                    }
                }
            }
            Err(error) => {
                apl_trigger.reset();
                log::error!("Luminance calculation failed: {error}");
            }
        }
    })?;

    // Camera exit drops the sender, allowing JPEG to finish its outstanding job.
    // Join both before propagating errors so a worker is not left detached.
    let capture_result = capture_thread.join();
    let jpeg_result = jpeg_thread.join();
    capture_result.map_err(|_| std::io::Error::other("camera_capture thread panicked"))??;
    jpeg_result.map_err(|_| std::io::Error::other("jpeg_encode thread panicked"))??;
    Ok(())
}
