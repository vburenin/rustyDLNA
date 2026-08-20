fn main() {
    println!("cargo:rerun-if-changed=src/ffmpeg_compat.c");
    cc::Build::new()
        .file("src/ffmpeg_compat.c")
        .warnings(true)
        .compile("rusty_dlna_ffmpeg_compat");
}
