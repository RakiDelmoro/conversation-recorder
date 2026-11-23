# syntax=docker/dockerfile:1
# ---------------------------------------------------------------------------
# conversation-recorder — ESP32-S3 firmware dev container
# Base: official Espressif ESP-IDF image (Debian + xtensa/riscv toolchains,
# esptool, openocd, idf-component-manager preinstalled).
#
# No wake word in this design: speech is detected by a VAD / energy gate on the
# XVF3800's processed (AEC + noise-suppressed + beamformed) I2S channel, so no
# WakeNet model is baked into the image. esp-sr (optional VAD) is a managed
# component fetched by `idf.py build` on first build — nothing to add here.
# ---------------------------------------------------------------------------
ARG IDF_VERSION=release-v5.4
FROM espressif/idf:${IDF_VERSION}

#Debian
ENV DEBIAN_FRONTEND=noninteractive

# --- board / audio / network / debugging helpers ---------------------------
RUN apt-get update && apt-get install -y --no-install-recommends \
        picocom \
        minicom \
        usbutils \
        udev \
        dfu-util \
        clang-format \
        gdb-multiarch \
        alsa-utils \
        sox \
        python3-numpy \
        python3-matplotlib \
        socat \
        netcat-openbsd \
        tcpdump \
        iproute2 \
        iputils-ping \
        dnsutils \
        avahi-utils \
        mosquitto-clients \
        openssl \
        jq \
        shellcheck \
    && rm -rf /var/lib/apt/lists/*

# --- make `idf.py` available in every shell --------------------------------
# The base image only exports the toolchain from its ENTRYPOINT, so shells
# opened by VS Code / `docker exec` would otherwise not find idf.py.
RUN printf '%s\n' \
      '# ESP-IDF environment' \
      'if [ -z "${IDF_EXPORTED:-}" ] && [ -f /opt/esp/idf/export.sh ]; then' \
      '  export IDF_PATH=/opt/esp/idf' \
      '  . /opt/esp/idf/export.sh >/dev/null 2>&1' \
      '  export IDF_EXPORTED=1' \
      'fi' \
      > /etc/profile.d/10-esp-idf.sh \
    && chmod 0644 /etc/profile.d/10-esp-idf.sh \
    && printf '%s\n' \
      '[ -f /etc/profile.d/10-esp-idf.sh ] && . /etc/profile.d/10-esp-idf.sh' \
      >> /etc/bash.bashrc

# --- let the non-root dev user extend / repair the IDF tool install --------
# (e.g. `idf_tools.py install esp-elf-gdb`, or extra managed components)
RUN chmod -R a+rwX /opt/esp/tools /opt/esp/python_env 2>/dev/null || true

# No USER / ENTRYPOINT on purpose: devcontainer.json decides who runs.