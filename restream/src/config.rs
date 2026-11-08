//! Конфиг `/etc/restream/config.ini`.
//!
//! Небольшой разборщик INI: секции `[имя]`, строки `ключ = значение`, `#` и `;` — комментарии.

use std::collections::BTreeMap;
use std::path::Path;

pub type Section = BTreeMap<String, String>;

#[derive(Debug, Default)]
pub struct Ini {
    pub sections: BTreeMap<String, Section>,
}

impl Ini {
    pub fn parse(text: &str) -> Self {
        let mut ini = Ini::default();
        let mut current = String::from("general");
        ini.sections.insert(current.clone(), Section::new());
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(rest) = line.strip_prefix('[') {
                if let Some(name) = rest.strip_suffix(']') {
                    current = name.trim().to_string();
                    ini.sections.entry(current.clone()).or_default();
                    continue;
                }
            }
            if let Some((k, v)) = line.split_once('=') {
                let key = k.trim().to_string();
                let mut val = v.trim().to_string();
                // Инлайновый комментарий: "  # ..." или "  ; ..."
                for marker in [" #", " ;"] {
                    if let Some(pos) = val.find(marker) {
                        val.truncate(pos);
                    }
                }
                ini.sections
                    .entry(current.clone())
                    .or_default()
                    .insert(key, val.trim().to_string());
            }
        }
        ini
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections
            .get(section)
            .and_then(|s| s.get(key))
            .map(|s| s.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Output {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub live_path: String,
    pub fallback_file: String,
    pub ffmpeg: String,
    pub control_host: String,
    pub control_port: u16,

    pub listen_host: String,
    pub listen_port: u16,
    pub mtx_host: String,
    pub mtx_port: u16,
    pub mtx_rtmp: String,
    pub mtx_api: String,

    /// Включён ли приём SRT.
    pub srt_enabled: bool,
    pub srt_host: String,
    pub srt_port: u16,
    /// Адрес SRT в MediaMTX, куда пересылаются датаграммы.
    pub srt_upstream_host: String,
    pub srt_upstream_port: u16,

    pub live_confirm_secs: f64,
    pub fallback_max_secs: f64,
    pub stall_secs: f64,
    pub poll_secs: f64,
    pub quiet_ffmpeg: bool,

    pub outputs: Vec<Output>,
}

fn f64_or(v: Option<&str>, d: f64) -> f64 {
    v.and_then(|s| s.trim().parse::<f64>().ok()).unwrap_or(d)
}

fn u16_or(v: Option<&str>, d: u16) -> u16 {
    v.and_then(|s| s.trim().parse::<u16>().ok()).unwrap_or(d)
}

fn bool_or(v: Option<&str>, d: bool) -> bool {
    match v.map(|s| s.trim().to_ascii_lowercase()) {
        Some(s) if matches!(s.as_str(), "yes" | "true" | "1" | "on") => true,
        Some(s) if matches!(s.as_str(), "no" | "false" | "0" | "off") => false,
        _ => d,
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("не читается конфиг {}: {}", path.display(), e))?;
        let ini = Ini::parse(&text);

        let live_path = ini
            .get("general", "live_path")
            .map(|s| s.trim_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "в конфиге не задан live_path".to_string())?;

        let mtx_port = u16_or(ini.get("general", "mediamtx_port"), 11935);

        let mut outputs = Vec::new();
        for (name, section) in &ini.sections {
            let Some(short) = name.strip_prefix("output.") else {
                continue;
            };
            if !bool_or(section.get("enabled").map(|s| s.as_str()), true) {
                continue;
            }
            let Some(url) = section.get("url").map(|s| s.trim()).filter(|s| !s.is_empty()) else {
                continue;
            };
            // Незаполненная заготовка из install.sh не должна превращаться в выход.
            if url.contains("ВСТАВЬ") {
                continue;
            }
            outputs.push(Output {
                name: short.to_string(),
                url: url.to_string(),
            });
        }

        Ok(Config {
            live_path,
            fallback_file: ini
                .get("general", "fallback_file")
                .unwrap_or("/var/lib/restream/fallback.mp4")
                .to_string(),
            ffmpeg: ini.get("general", "ffmpeg").unwrap_or("ffmpeg").to_string(),
            control_host: ini
                .get("general", "control_host")
                .unwrap_or("127.0.0.1")
                .to_string(),
            control_port: u16_or(ini.get("general", "control_port"), 8088),

            listen_host: ini.get("general", "listen_host").unwrap_or("0.0.0.0").to_string(),
            listen_port: u16_or(ini.get("general", "listen_port"), 1935),
            mtx_host: ini
                .get("general", "mediamtx_host")
                .unwrap_or("127.0.0.1")
                .to_string(),
            mtx_port,
            mtx_rtmp: ini
                .get("general", "mediamtx_rtmp")
                .map(|s| s.trim_end_matches('/').to_string())
                .unwrap_or_else(|| format!("rtmp://127.0.0.1:{}", mtx_port)),
            mtx_api: ini
                .get("general", "mediamtx_api")
                .map(|s| s.trim().trim_end_matches('/').to_string())
                .unwrap_or_else(|| "http://127.0.0.1:9997".to_string()),

            srt_enabled: bool_or(ini.get("general", "srt_enabled"), true),
            srt_host: ini.get("general", "srt_listen_host").unwrap_or("0.0.0.0").to_string(),
            srt_port: u16_or(ini.get("general", "srt_listen_port"), 8890),
            srt_upstream_host: ini
                .get("general", "srt_upstream_host")
                .unwrap_or("127.0.0.1")
                .to_string(),
            srt_upstream_port: u16_or(ini.get("general", "srt_upstream_port"), 18890),

            live_confirm_secs: f64_or(ini.get("general", "live_confirm_seconds"), 2.0),
            fallback_max_secs: f64_or(ini.get("general", "fallback_max_minutes"), 30.0) * 60.0,
            stall_secs: f64_or(ini.get("general", "stall_timeout"), 6.0),
            poll_secs: f64_or(ini.get("general", "poll_interval"), 0.5),
            quiet_ffmpeg: bool_or(ini.get("general", "quiet_ffmpeg"), false),

            outputs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[general]
live_path = live/abc
fallback_max_minutes = 0
stall_timeout = 3.0
control_port = 8099
# комментарий
[output.test]
enabled = no
url = rtmp://127.0.0.1:11935/test/key

[output.twitch]
enabled = yes
url = rtmp://live.twitch.tv/app/secret  # боевой

[output.kick]
enabled = yes
url = rtmps://host:443/app/ВСТАВЬ_КЛЮЧ_KICK
"#;

    #[test]
    fn parses_general() {
        let ini = Ini::parse(SAMPLE);
        assert_eq!(ini.get("general", "live_path"), Some("live/abc"));
        assert_eq!(ini.get("general", "control_port"), Some("8099"));
    }

    #[test]
    fn only_enabled_and_filled_outputs() {
        let dir = std::env::temp_dir().join("restream-cfg-test.ini");
        std::fs::write(&dir, SAMPLE).unwrap();
        let cfg = Config::load(&dir).unwrap();
        let names: Vec<_> = cfg.outputs.iter().map(|o| o.name.as_str()).collect();
        // test выключен, kick содержит заготовку
        assert_eq!(names, vec!["twitch"]);
        assert_eq!(cfg.outputs[0].url, "rtmp://live.twitch.tv/app/secret");
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn fallback_max_zero_means_never() {
        let dir = std::env::temp_dir().join("restream-cfg-test2.ini");
        std::fs::write(&dir, SAMPLE).unwrap();
        let cfg = Config::load(&dir).unwrap();
        assert_eq!(cfg.fallback_max_secs, 0.0);
        assert_eq!(cfg.stall_secs, 3.0);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn srt_defaults_and_overrides() {
        let dir = std::env::temp_dir().join("restream-cfg-srt.ini");
        std::fs::write(&dir, SAMPLE).unwrap();
        let cfg = Config::load(&dir).unwrap();
        assert!(cfg.srt_enabled);
        assert_eq!(cfg.srt_port, 8890);
        assert_eq!(cfg.srt_upstream_port, 18890);
        let _ = std::fs::remove_file(&dir);

        // Дописываем в [general] — иначе ключи попадут в последнюю секцию файла.
        let with_srt = SAMPLE.replace(
            "[general]\n",
            "[general]\nsrt_enabled = no\nsrt_listen_port = 9000\n",
        );
        std::fs::write(&dir, with_srt).unwrap();
        let cfg = Config::load(&dir).unwrap();
        assert!(!cfg.srt_enabled);
        assert_eq!(cfg.srt_port, 9000);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn inline_comment_is_stripped() {
        let ini = Ini::parse(SAMPLE);
        assert_eq!(
            ini.get("output.twitch", "url"),
            Some("rtmp://live.twitch.tv/app/secret")
        );
    }
}
