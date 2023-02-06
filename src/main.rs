use argh::FromArgs;
use heim::{cpu, disk, host, memory};
use mqtt_async_client::client::{Client, Publish};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use thiserror::Error;
use tokio::{fs, signal, time};
use url::Url;

const KEYRING_SERVICE_NAME: &str = "system-mqtt";

#[derive(FromArgs)]
/// Push system statistics to an mqtt server.
struct Arguments {
    /// the configuration file we are to use.
    #[argh(option, default = "PathBuf::from(\"/etc/system-mqtt.yaml\")")]
    config_file: PathBuf,

    #[argh(subcommand)]
    command: SubCommand,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand)]
enum SubCommand {
    Run(RunArguments),
    SetPassword(SetPasswordArguments),
}

#[derive(FromArgs, PartialEq, Debug)]
/// Run the daemon.
#[argh(subcommand, name = "run")]
struct RunArguments {}

#[derive(FromArgs, PartialEq, Debug)]
/// Set the password used to log into the mqtt client.
#[argh(subcommand, name = "set-password")]
struct SetPasswordArguments {}

#[derive(Serialize, Deserialize)]
struct DriveConfig {
    path: PathBuf,
    name: String,
}

#[derive(Serialize, Deserialize)]
struct Config {
    /// The URL of the mqtt server.
    mqtt_server: Url,

    /// Set the username to connect to the mqtt server, if required.
    /// The password will be fetched from the OS keyring.
    username: Option<String>,

    /// The interval to update at.
    update_interval: Duration,

    /// The names of drives, or the paths to where they are mounted.
    drives: Vec<DriveConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mqtt_server: Url::parse("mqtt://localhost").expect("Failed to parse default URL."),
            username: None,
            update_interval: Duration::from_secs(30),
            drives: vec![DriveConfig {
                path: PathBuf::from("/"),
                name: String::from("root"),
            }],
        }
    }
}

#[derive(Error, Debug)]
enum Error {
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Yaml encoding error: {0}")]
    YamlEncoding(#[from] serde_yaml::Error),

    #[error("Json encoding error: {0}")]
    JsonEncoding(#[from] serde_json::Error),

    #[error("You must set the username for login with the mqtt server before you can set the user's password")]
    CredentialsNotEnabled,

    #[error(
        "Keyring Error: {0}\nIf you have not yet set the password run `system-mqtt set-password`."
    )]
    Keyring(#[from] keyring::KeyringError),

    #[error("Error with mqtt protocol: {0}")]
    Mqtt(#[from] mqtt_async_client::Error),

    #[error("Failed to fetch system info: {0}")]
    SystemInfo(#[from] heim::Error),

    #[error("Failed to read battery info: {0}")]
    Battery(#[from] battery::Error),
}

type Result<T> = std::result::Result<T, Error>;

#[tokio::main]
async fn main() {
    let arguments: Arguments = argh::from_env();

    match load_config(&arguments.config_file).await {
        Ok(config) => match arguments.command {
            SubCommand::Run(_arguments) => {
                mowl::init_with_level(log::LevelFilter::Info).expect("Failed to setup log.");

                loop {
                    if let Err(error) = application_trampoline(&config).await {
                        log::error!("Fatal error: {}", error);
                    } else {
                        // This is a graceful shutdown.
                        break;
                    }
                }
            }
            SubCommand::SetPassword(_arguments) => {
                if let Err(error) = set_password(config).await {
                    eprintln!("Fatal error: {}", error);
                }
            }
        },
        Err(error) => {
            eprintln!("Failed to load config file: {}", error);
        }
    }
}

async fn load_config(path: &Path) -> Result<Config> {
    if path.is_file() {
        // It's a readable file we can load.

        let config: Config = serde_yaml::from_str(&fs::read_to_string(path).await?)?;

        Ok(config)
    } else {
        // Doesn't exist yet. We'll create it.
        let config = Config::default();

        // Write it to a file for next time we load.
        fs::write(path, serde_yaml::to_string(&config)?).await?;

        Ok(config)
    }
}

async fn set_password(config: Config) -> Result<()> {
    if let Some(username) = config.username {
        let password = rpassword::read_password_from_tty(Some("Password: "))?;

        let keyring = keyring::Keyring::new(KEYRING_SERVICE_NAME, &username);
        keyring.set_password(&password)?;

        Ok(())
    } else {
        Err(Error::CredentialsNotEnabled)
    }
}

async fn application_trampoline(config: &Config) -> Result<()> {
    let mut client_builder = Client::builder();
    client_builder.set_url_string(config.mqtt_server.as_str())?;

    // If credentials are provided, use them.
    if let Some(username) = &config.username {
        // TODO make TLS mandatory when using this.

        let keyring = keyring::Keyring::new(KEYRING_SERVICE_NAME, &username);
        let password = keyring.get_password()?;

        client_builder.set_username(Some(username.into()));
        client_builder.set_password(Some(password.into()));
    }

    let mut client = client_builder.build()?;
    client.connect().await?;

    let platform = host::platform().await?;
    let hostname = platform.hostname();

    async fn register_topic(
        client: &mut Client,
        hostname: &str,
        topic_class: &str,
        device_class: Option<&str>,
        topic_name: &str,
        unit_of_measurement: Option<&str>,
        icon: Option<&str>,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct TopicConfig {
            name: String,

            #[serde(skip_serializing_if = "Option::is_none")]
            device_class: Option<String>,
            state_topic: String,
            unit_of_measurement: Option<String>,
            icon: Option<String>,
            unique_id: String,
        }

        let message = serde_json::ser::to_string(&TopicConfig {
            name: format!("{}-{}", hostname, topic_name),
            device_class: device_class.map(str::to_string),
            state_topic: format!("system-mqtt/{}/{}", hostname, topic_name),
            unit_of_measurement: unit_of_measurement.map(str::to_string),
            icon: icon.map(str::to_string),
            unique_id: format!("system-mqtt-{}-{}", hostname, topic_name)
        })?;
        let mut publish = Publish::new(
            format!(
                "homeassistant/{}/system-mqtt-{}/{}/config",
                topic_class, hostname, topic_name
            ),
            message.into(),
        );
        publish.set_retain(true);
        client.publish(&publish).await?;
        Ok(())
    }

    async fn publish(
        client: &mut Client,
        hostname: &str,
        topic_name: &str,
        value: String,
    ) -> Result<()> {
        let mut publish = Publish::new(
            format!("system-mqtt/{}/{}", hostname, topic_name),
            value.into(),
        );
        publish.set_retain(false);
        client.publish(&publish).await?;

        Ok(())
    }

    // Register the various sensor topics and include the details about that sensor

    //    TODO - create a new register_topic to register binary_sensor so we can make availability a real binary sensor. In the
    //    meantime, create it as a normal analog sensor with two values, and a template can be used to make it a binary.

    register_topic(
        &mut client,
        hostname,
        "sensor",
        None,
        "available",
        None,
        Some("mdi:check-network-outline"),
    )
    .await?;
    register_topic(
        &mut client,
        hostname,
        "sensor",
        None,
        "uptime",
        Some("days"),
        Some("mdi:timer-sand"),
    )
    .await?;
    register_topic(
        &mut client,
        hostname,
        "sensor",
        None,
        "cpu",
        Some("%"),
        Some("mdi:gauge"),
    )
    .await?;
    register_topic(
        &mut client,
        hostname,
        "sensor",
        None,
        "memory",
        Some("%"),
        Some("mdi:gauge"),
    )
    .await?;
    register_topic(
        &mut client,
        hostname,
        "sensor",
        None,
        "swap",
        Some("%"),
        Some("mdi:gauge"),
    )
    .await?;

    // Register the sensors for filesystems
    for drive in &config.drives {
        register_topic(
            &mut client,
            hostname,
            "sensor",
            None,
            &drive.name,
            Some("%"),
            Some("mdi:folder"),
        )
        .await?;
    }

    client
        .publish(
            &Publish::new(
                format!("system-mqtt/{}/availability", hostname),
                "online".into(),
            )
            .set_retain(true),
        )
        .await?;

    let cpu_stats = cpu::time().await?;
    let mut previous_used_cpu_time = cpu_stats.user() + cpu_stats.system();
    let mut previous_total_cpu_time = previous_used_cpu_time + cpu_stats.idle();

    loop {
        tokio::select! {
            _ = time::sleep(config.update_interval) => {
                // Report uptime.
                let uptime = host::uptime().await?;
                publish(&mut client, &hostname, "uptime", uptime.get::<heim::units::time::day>().to_string()).await?;

                // Report CPU usage.
                let cpu_stats = cpu::time().await?;
                let used_cpu_time = cpu_stats.user() + cpu_stats.system();
                let total_cpu_time = used_cpu_time + cpu_stats.idle();

                let used_cpu_time_delta = used_cpu_time - previous_used_cpu_time;
                let total_cpu_time_delta = total_cpu_time - previous_total_cpu_time;

                previous_used_cpu_time = used_cpu_time;
                previous_total_cpu_time = total_cpu_time;

                let cpu_load_percentile = used_cpu_time_delta / total_cpu_time_delta;
                publish(&mut client, &hostname, "cpu", format!("{:.1}", cpu_load_percentile.get::<heim::units::ratio::ratio>().clamp(0.0, 1.0) * 100.0)).await?;

                // Report memory usage.
                let memory = memory::memory().await?;
                let memory_percentile = (memory.total().get::<heim::units::information::byte>() - memory.available().get::<heim::units::information::byte>()) as f64 / memory.total().get::<heim::units::information::byte>() as f64;
                publish(&mut client, &hostname, "memory", format!("{:.1}", memory_percentile.clamp(0.0, 1.0) * 100.0)).await?;

                // Report swap usage.
                let swap = memory::swap().await?;
                let swap_percentile = swap.used().get::<heim::units::information::byte>() as f64 / swap.total().get::<heim::units::information::byte>() as f64;
                publish(&mut client, &hostname, "swap", format!("{:.1}", swap_percentile.clamp(0.0, 1.0) * 100.0)).await?;

                // Report filesystem usage.
                for drive in &config.drives {
                    match disk::usage(&drive.path).await {
                        Ok(disk) => {
                            let drive_percentile = (disk.total().get::<heim::units::information::byte>() - disk.free().get::<heim::units::information::byte>()) as f64 / disk.total().get::<heim::units::information::byte>() as f64;

                            publish(&mut client, &hostname, &drive.name, format!("{:.1}", drive_percentile.clamp(0.0, 1.0) * 100.0)).await?;
                        },
                        Err(error) => {
                            log::warn!("Unable to read drive usage statistics: {}", error);
                        }
                    }
                }
            }
            _ = signal::ctrl_c() => {
                log::info!("Terminate signal has been received.");
                break;
            }
        }
    }

    client
        .publish(
            &Publish::new(
                format!("system-mqtt/{}/availability", hostname),
                "offline".into(),
            )
            .set_retain(true),
        )
        .await?;

    client.disconnect().await?;

    Ok(())
}
