//! Генерация файла заглушки через ffmpeg.

use crate::config::Config;
use std::process::Command;

const FONTS: [&str; 4] = [
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", // Debian/Ubuntu
    "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",             // Arch
    "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
];

struct Opts {
    image: Option<String>,
    video: Option<String>,
    text: String,
    seconds: u32,
    output: Option<String>,
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        image: None,
        video: None,
        text: "Скоро вернёмся".to_string(),
        seconds: 30,
        output: None,
    };
    let mut i = 0;
    while i < args.len() {
        let take = |i: usize, name: &str| -> Result<String, String> {
            args.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("после {} нужно значение", name))
        };
        match args[i].as_str() {
            "--image" => {
                o.image = Some(take(i, "--image")?);
                i += 2;
            }
            "--video" => {
                o.video = Some(take(i, "--video")?);
                i += 2;
            }
            "--text" => {
                o.text = take(i, "--text")?;
                i += 2;
            }
            "--seconds" => {
                o.seconds = take(i, "--seconds")?
                    .parse()
                    .map_err(|_| "--seconds: нужно целое число".to_string())?;
                i += 2;
            }
            "-o" | "--output" => {
                o.output = Some(take(i, "-o")?);
                i += 2;
            }
            other => return Err(format!("неизвестный параметр: {}", other)),
        }
    }
    Ok(o)
}

pub fn run(cfg: &Config, args: &[String]) -> Result<(), String> {
    let o = parse_opts(args)?;
    let out = o.output.clone().unwrap_or_else(|| cfg.fallback_file.clone());

    if let Some(parent) = std::path::Path::new(&out).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("не создать папку {}: {}", parent.display(), e))?;
    }

    let fit = "scale=1280:720:force_original_aspect_ratio=decrease,\
               pad=1280:720:(ow-iw)/2:(oh-ih)/2,setsar=1";

    let mut c = Command::new(&cfg.ffmpeg);
    c.args(["-y", "-hide_banner", "-loglevel", "error"]);

    let vf: String;
    if let Some(img) = &o.image {
        c.args(["-loop", "1", "-framerate", "30", "-i", img]);
        vf = fit.to_string();
    } else if let Some(vid) = &o.video {
        c.args(["-i", vid]);
        vf = fit.to_string();
    } else {
        c.args(["-f", "lavfi", "-i", "color=c=0x101820:s=1280x720:r=30"]);
        match FONTS.iter().find(|f| std::path::Path::new(f).is_file()) {
            Some(font) => {
                // Символы, ломающие фильтр ffmpeg, выкидываем: экранировать надёжнее,
                // чем вычищать последствия.
                let safe: String = o
                    .text
                    .chars()
                    .filter(|c| !matches!(c, ':' | '\'' | '\\' | '%' | ',' | ';' | '[' | ']' | '='))
                    .collect();
                vf = format!(
                    "drawtext=fontfile={}:text={}:fontcolor=white:fontsize=72:x=(w-text_w)/2:y=(h-text_h)/2",
                    font, safe
                );
            }
            None => {
                eprintln!(
                    "шрифт DejaVu не найден — заглушка будет без текста \
                     (поставь fonts-dejavu-core / ttf-dejavu)"
                );
                vf = "null".to_string();
            }
        }
    }

    c.args(["-f", "lavfi", "-i", "anullsrc=r=44100:cl=stereo"])
        .args(["-map", "0:v:0", "-map", "1:a:0"])
        .args(["-t", &o.seconds.to_string(), "-shortest"])
        .args(["-vf", &format!("{},format=yuv420p,fps=30", vf)])
        .args(["-c:v", "libx264", "-preset", "veryfast", "-profile:v", "main"])
        .args(["-b:v", "1500k", "-maxrate", "1500k", "-bufsize", "3000k"])
        .args(["-g", "60", "-keyint_min", "60", "-sc_threshold", "0"])
        .args(["-c:a", "aac", "-b:a", "128k", "-ar", "44100", "-ac", "2"])
        .args(["-movflags", "+faststart"])
        .arg(&out);

    let status = c
        .status()
        .map_err(|e| format!("не удалось запустить ffmpeg: {}", e))?;
    if !status.success() {
        return Err(format!("ffmpeg завершился с ошибкой ({})", status));
    }
    println!("Готово: {}", out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn defaults() {
        let o = parse_opts(&s(&[])).unwrap();
        assert_eq!(o.seconds, 30);
        assert_eq!(o.text, "Скоро вернёмся");
        assert!(o.image.is_none());
    }

    #[test]
    fn parses_flags() {
        let o = parse_opts(&s(&["--text", "Привет", "--seconds", "5", "-o", "/tmp/x.mp4"])).unwrap();
        assert_eq!(o.text, "Привет");
        assert_eq!(o.seconds, 5);
        assert_eq!(o.output.as_deref(), Some("/tmp/x.mp4"));
    }

    #[test]
    fn missing_value_is_error() {
        assert!(parse_opts(&s(&["--text"])).is_err());
        assert!(parse_opts(&s(&["--seconds", "abc"])).is_err());
        assert!(parse_opts(&s(&["--nope"])).is_err());
    }
}
