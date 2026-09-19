use std::path::Path;
use std::process::Command;

fn main() {
    let dist_dir = Path::new("../frontend/dist");
    let index_file = dist_dir.join("index.html");

    // If frontend/dist doesn't exist, try building it with npm if available
    if !index_file.is_file() {
        let npm_status = Command::new("npm")
            .args(["--prefix", "../frontend", "run", "build"])
            .status();

        if npm_status.is_err() || !npm_status.unwrap().success() {
            let _ = std::fs::create_dir_all(dist_dir);
            let fallback_html = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>AI Remote</title>
</head>
<body>
  <h1>AI Remote 信令服务已就绪</h1>
  <p>前端静态文件尚未构建。请访问 <a href="/setup">/setup</a> 控制面板，或运行 <code>npm run build</code> 构建完整前端。</p>
</body>
</html>"#;
            let _ = std::fs::write(&index_file, fallback_html);
        }
    }

    println!("cargo:rerun-if-changed=../frontend/dist");
}
