use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const DEFAULT_SOCKET: &str = "/tmp/waybar-damon";
const DEFAULT_DATA_FILE: &str = "/tmp/waybar-daemon-data.json";
const DEFAULT_CONFIG: &str = "waybar-net-daemon.yaml";
const DEFAULT_INTERVAL_SECS: u64 = 15;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

type Cache = Arc<Mutex<HashMap<String, String>>>;

#[derive(Debug, Clone, Deserialize)]
struct Config {
    keys: Vec<String>,
    urls: Vec<String>,
}

impl Config {
    fn map(&self) -> Vec<(String, String)> {
        self.keys
            .iter()
            .cloned()
            .zip(self.urls.iter().cloned())
            .filter(|(k, u)| !k.is_empty() && !u.is_empty())
            .collect()
    }
}

fn parse_config(contents: &str) -> Result<Config, yaml_serde::Error> {
    yaml_serde::from_str(contents)
}

fn find_config(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    if let Some(p) = std::env::var_os("WAYBAR_NET_DAEMON_CONFIG") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let pb = dir.join(DEFAULT_CONFIG);
            if pb.is_file() {
                return Some(pb);
            }
        }
    }
    let pb = PathBuf::from(DEFAULT_CONFIG);
    if pb.is_file() {
        return Some(pb);
    }
    return None;
}

fn http_get(url: &str) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build();
    match agent.get(url).call() {
        Ok(resp) => {
            if resp.status() != http::StatusCode::OK {
                return Err(format!("HTTP {} for {url}", resp.status()));
            }
            resp.into_string()
                .map_err(|e| format!("read body for {url}: {e}"))
        }
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code} for {url}")),
        Err(e) => Err(format!("request failed for {url}: {e}")),
    }
}

fn load_cache_from_disk(path: &Path, cache: &Cache) {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let data: HashMap<String, String> = match serde_json::from_str(&contents) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "[waybar-net-daemon] failed to parse cache {}: {e}",
                path.display()
            );
            return;
        }
    };
    let mut guard = cache.lock().unwrap();
    *guard = data;
}

fn persist_cache(path: &Path, cache: &Cache) {
    let serialized = {
        let guard = cache.lock().unwrap();
        serde_json::to_string_pretty(&*guard)
    };
    match serialized {
        Ok(s) => {
            let tmp = path.with_extension("tmp");
            if let Err(e) = fs::write(&tmp, s) {
                eprintln!("[waybar-net-daemon] failed to write cache: {e}");
                return;
            }
            if let Err(e) = fs::rename(&tmp, path) {
                eprintln!("[waybar-net-daemon] failed to persist cache: {e}");
            }
        }
        Err(e) => eprintln!("[waybar-net-daemon] failed to serialize cache: {e}"),
    }
}

fn refresh_once(entries: &[(String, String)], data_path: &Path, cache: &Cache) {
    let mut changed = false;
    for (key, url) in entries {
        match http_get(url) {
            Ok(body) => {
                cache.lock().unwrap().insert(key.clone(), body);
                changed = true;
            }
            Err(e) => {
                eprintln!("[waybar-net-daemon] fetch failed for key '{key}' ({url}): {e}");
            }
        }
    }
    if changed {
        persist_cache(data_path, cache);
    }
}

fn refresh_loop(
    entries: Vec<(String, String)>,
    data_path: PathBuf,
    cache: Cache,
    interval: Duration,
) {
    refresh_once(&entries, &data_path, &cache);
    loop {
        thread::sleep(interval);
        refresh_once(&entries, &data_path, &cache);
    }
}

fn handle_client(mut stream: UnixStream, cache: &Cache) {
    let mut reader = BufReader::new(&mut stream);
    let mut key = String::new();
    if reader.read_line(&mut key).is_err() {
        return;
    }
    let key = key.trim();

    let body = cache.lock().unwrap().get(key).cloned();
    match body {
        Some(body) => {
            let _ = stream.write_all(b"OK\n");
            let _ = stream.write_all(body.as_bytes());
        }
        None => {
            let msg = format!("ERR unknown key: {key}\n");
            let _ = stream.write_all(msg.as_bytes());
        }
    }
    let _ = stream.flush();
}

fn serve_socket(socket_path: &str, cache: Cache) {
    let _ = fs::remove_file(socket_path);
    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[waybar-net-daemon] failed to bind socket {socket_path}: {e}");
            std::process::exit(1);
        }
    };

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let cache = Arc::clone(&cache);
                thread::spawn(move || handle_client(stream, &cache));
            }
            Err(e) => eprintln!("[waybar-net-daemon] accept error: {e}"),
        }
    }
}

fn load_config_file() -> (PathBuf, String) {
    let args: Vec<String> = std::env::args().collect();
    let explicit_config = args.get(1).map(String::as_str);
    let config_path = match find_config(explicit_config) {
        Some(p) => p,
        None => {
            eprintln!(
                "[waybar-net-daemon] config file not found; searched env, exe dir and cwd for {DEFAULT_CONFIG}"
            );
            std::process::exit(1);
        }
    };
    let contents = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[waybar-net-daemon] failed to read config {}: {e}",
                config_path.display()
            );
            std::process::exit(1);
        }
    };
    return (config_path, contents);
}

fn load_config() -> Vec<(String, String)> {
    let (config_path, contents) = load_config_file();
    let cfg = match parse_config(&contents) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[waybar-net-daemon] failed to parse config {}: {e}",
                config_path.display()
            );
            std::process::exit(1);
        }
    };
    let entries = cfg.map();
    if entries.is_empty() {
        eprintln!(
            "[waybar-net-daemon] config {} has no url/key pairs",
            config_path.display()
        );
        std::process::exit(1);
    }
    return entries;
}

fn load_interval() -> u64 {
    return std::env::var("WAYBAR_NET_DAEMON_INTERVAL")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_INTERVAL_SECS);
}

fn main() {
    let cache: Cache = Arc::new(Mutex::new(HashMap::new()));
    let data_path = PathBuf::from(DEFAULT_DATA_FILE);
    let interval = load_interval();
    let entries = load_config();

    load_cache_from_disk(&data_path, &cache);
    eprintln!(
        "[waybar-net-daemon] cache loaded from {}",
        data_path.display()
    );

    let cache_for_loop = Arc::clone(&cache);
    let data_for_loop = data_path.clone();
    let entries_for_loop = entries.clone();
    thread::spawn(move || {
        refresh_loop(
            entries_for_loop,
            data_for_loop,
            cache_for_loop,
            Duration::from_secs(interval),
        );
    });

    serve_socket(DEFAULT_SOCKET, cache);
}
