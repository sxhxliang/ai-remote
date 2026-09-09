use std::{env, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = PathBuf::from(
        env::args()
            .nth(1)
            .ok_or("Usage: dev-cert OUTPUT_DIRECTORY")?,
    );
    fs::create_dir_all(&directory)?;
    let cert_path = directory.join("cert.pem");
    let key_path = directory.join("key.pem");
    if cert_path.exists() || key_path.exists() {
        return Err("Refusing to overwrite existing certificates".into());
    }
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])?;
    fs::write(cert_path, cert.cert.pem())?;
    fs::write(key_path, cert.key_pair.serialize_pem())?;
    println!("Development certificate created; not for public deployment");
    Ok(())
}
