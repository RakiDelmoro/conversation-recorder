fn main() {
    embuild::espidf::sysenv::output();

    let mut build = cc::Build::new();
    build
        .include("src/ten_vad")
        .flag_if_supported("-std=c99")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-variable")
        .define("NDEBUG", None)
        .warnings(false)
        .files([
            "src/ten_vad/aed.c",
            "src/ten_vad/aivad_model.c",
            "src/ten_vad/biquad.c",
            "src/ten_vad/fftw.c",
            "src/ten_vad/pitch_est.c",
            "src/ten_vad/stft.c",
            "src/ten_vad/ten_vad.c",
        ]);

    if std::env::var("TARGET")
        .unwrap_or_default()
        .contains("xtensa")
    {
        let mcu = std::env::var("MCU").unwrap_or_else(|_| "esp32s3".into());
        build.compiler(format!("xtensa-{mcu}-elf-gcc"));
    }

    build.compile("ten_vad");
    println!("cargo:rustc-link-lib=m");
}
