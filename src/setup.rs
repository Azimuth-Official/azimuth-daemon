use std::io::{self, Write};
use std::path::PathBuf;

use crate::api_client::ApiClient;
use crate::config::{
    ApiConfig, Config, DaemonConfig, LocationConfig, NodeConfig, ScanningConfig, SdrConfig,
};

/// Arguments that can be pre-filled to skip interactive prompts.
#[derive(Default)]
pub struct SetupArgs {
    pub api_key: Option<String>,
    pub node_name: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub output_path: Option<String>,
    pub base_url: Option<String>,
}

const DEFAULT_BASE_URL: &str = "https://azimuth.day";

pub async fn run_setup(args: SetupArgs) -> anyhow::Result<PathBuf> {
    println!();
    println!("  ╔══════════════════════════════════════╗");
    println!("  ║  Azimuth Daemon — First-Run Setup    ║");
    println!("  ╚══════════════════════════════════════╝");
    println!();

    let output_path = PathBuf::from(
        args.output_path
            .as_deref()
            .unwrap_or("config.toml"),
    );

    // Check for existing config
    if output_path.exists() {
        println!("  Config file already exists: {}", output_path.display());
        let overwrite = prompt_yn("  Overwrite?", false)?;
        if !overwrite {
            println!("  Setup cancelled.");
            anyhow::bail!("Config file exists — user declined overwrite");
        }
        println!();
    }

    let base_url = args
        .base_url
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

    // Step 1: API Key
    println!("  Step 1/4: API Key");
    println!("  Sign up at https://azimuth.day and copy your API key from Account Settings.");
    let has_presets = args.api_key.is_some();
    let api_key = match args.api_key {
        Some(k) => {
            println!("  API key: ****{}", &k[k.len().saturating_sub(4)..]);
            k
        }
        None => prompt("  Paste API key")?,
    };
    if api_key.is_empty() {
        anyhow::bail!("API key cannot be empty");
    }

    let client = ApiClient::new(&base_url, &api_key);
    println!("  Verifying API key...");

    // Step 2: Node Registration
    println!();
    println!("  Step 2/4: Node Registration");
    let node_name = match args.node_name {
        Some(n) => {
            println!("  Node name: {}", n);
            n
        }
        None => prompt_default("  Node name", "my-azimuth-node")?,
    };

    let lat = match args.latitude {
        Some(l) => {
            println!("  Latitude: {}", l);
            l
        }
        None => {
            println!();
            println!("  Step 3/4: Node Location");
            prompt_f64("  Latitude", 0.0)?
        }
    };
    let lon = match args.longitude {
        Some(l) => {
            println!("  Longitude: {}", l);
            l
        }
        None => prompt_f64("  Longitude", 0.0)?,
    };

    println!("  Registering node...");
    let node_id = client
        .register_node("tier1_rtlsdr", &node_name, lat, lon, 0.0)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Node registration failed: {}. Check your API key and try again.",
                e
            )
        })?;
    println!("  Node ID: {} ✓", node_id);

    // Step 4: SDR Configuration
    println!();
    println!("  Step 4/4: SDR Configuration");
    let gain = if has_presets {
        // Automated mode — use auto gain
        0
    } else {
        println!("  Gain mode:");
        println!("    [1] Auto (AGC) — recommended");
        println!("    [2] Manual");
        let choice = prompt_default("  Choice", "1")?;
        if choice == "2" {
            let g = prompt_default("  Gain (dB, 0-50)", "40")?;
            g.parse::<i32>().unwrap_or(40)
        } else {
            0
        }
    };

    let scan_mode = if has_presets {
        "both".to_string()
    } else {
        println!("  Scan mode:");
        println!("    [1] Config only — scan preset frequencies");
        println!("    [2] Auto-discover — sweep bands to find stations");
        println!("    [3] Both — recommended");
        let choice = prompt_default("  Choice", "3")?;
        match choice.as_str() {
            "1" => "config".to_string(),
            "2" => "sweep".to_string(),
            _ => "both".to_string(),
        }
    };

    // Build config
    let config = Config {
        api: ApiConfig {
            url: base_url,
            key: api_key,
        },
        node: NodeConfig {
            id: node_id,
            hardware_type: "tier1_rtlsdr".to_string(),
            label: node_name,
            location: LocationConfig {
                latitude: lat,
                longitude: lon,
                altitude_m: 0.0,
            },
        },
        daemon: DaemonConfig {
            heartbeat_interval_secs: 15,
            submit_interval_secs: 30,
            status_bind: "127.0.0.1:8420".to_string(),
        },
        sdr: SdrConfig {
            device_index: None,
            sample_rate: 2_048_000,
            center_frequency: 100_000_000,
            gain,
            dbm_offset: -30.0,
        },
        scanning: Some(ScanningConfig {
            mode: scan_mode,
            ..Default::default()
        }),
        ntrip: None,
    };

    // Write config
    println!();
    println!("  Step 5/5: Writing Configuration");
    config.save(&output_path)?;
    println!("  Config written to: {} ✓", output_path.display());

    println!();
    println!("  ┌─────────────────────────────────────────┐");
    println!("  │  Setup complete! Start mining with:      │");
    println!("  │  > azimuth-daemon                        │");
    println!("  │                                          │");
    println!("  │  Or install as Windows service:          │");
    println!("  │  > azimuth-daemon --install-service      │");
    println!("  └─────────────────────────────────────────┘");
    println!();

    Ok(output_path)
}

fn prompt(label: &str) -> io::Result<String> {
    print!("{}: ", label);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

fn prompt_default(label: &str, default: &str) -> io::Result<String> {
    print!("{} [{}]: ", label, default);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn prompt_f64(label: &str, default: f64) -> io::Result<f64> {
    let s = prompt_default(label, &format!("{}", default))?;
    Ok(s.parse::<f64>().unwrap_or(default))
}

fn prompt_yn(label: &str, default: bool) -> io::Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    print!("{} [{}]: ", label, hint);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().to_lowercase();
    if trimmed.is_empty() {
        Ok(default)
    } else {
        Ok(trimmed.starts_with('y'))
    }
}
