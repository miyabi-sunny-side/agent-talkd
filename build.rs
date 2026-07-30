use std::{
    env,
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
};

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-changed=client/dist");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set"));
    let dist = Path::new("client/dist");
    let mut files = Vec::new();
    if dist.is_dir() {
        collect(dist, dist, &mut files)?;
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut generated = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    for (route, path) in files {
        let path = path.canonicalize()?;
        writeln!(
            generated,
            "    ({route:?}, include_bytes!({path:?})),",
            path = path.to_string_lossy()
        )
        .expect("writing to a String cannot fail");
    }
    generated.push_str("];\n");
    fs::write(out.join("web_assets.rs"), generated)
}

fn collect(root: &Path, directory: &Path, files: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, files)?;
        } else if path.is_file() {
            let relative = path.strip_prefix(root).expect("asset is below dist");
            let route = format!("/{}", relative.to_string_lossy().replace('\\', "/"));
            files.push((route, path));
        }
    }
    Ok(())
}
