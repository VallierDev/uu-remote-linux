{
  lib,
  rustPlatform,
  pkg-config,
  wayland-scanner,
  makeBinaryWrapper,
  libx11,
  libxext,
  libxtst,
  libxfixes,
  gtk3,
  wayland,
  libxkbcommon,
  glib,
  pipewire,
  libei,
  SDL2,
  linux-pam,
  libpulseaudio,
  libopus,
  libgbm,
  libayatana-appindicator,
  libxcb,
  libva,
  ffmpeg-full,
  writableTmpDirAsHomeHook,
  withWayland ? true,
  withPortal ? true,
  withViewer ? true,
  withInprocAv ? false,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = finalAttrs.passthru.cargoToml.package.name;
  inherit (finalAttrs.passthru.cargoToml.package) version;

  src = lib.fileset.toSource {
    root = ../..;
    fileset = lib.fileset.unions [
      ../../Cargo.toml
      ../../Cargo.lock
      ../../build.rs
      ../../src
      ../../c_engine
      ../../vendor
      ../../benches
      ../../tests
      ../common
    ];
  };

  cargoLock.lockFile = ../../Cargo.lock;

  # Release feature set; the engines are linked in instead of being runtime plugins
  # (engine-plugins only exists to keep FHS packages free of Wayland/portal libraries).
  buildFeatures =
    lib.optional withWayland "wayland"
    ++ lib.optional withPortal "portal"
    ++ lib.optionals withViewer [
      "native-viewer"
      "viewer-sdl"
    ]
    ++ lib.optional withInprocAv "inproc-av";

  nativeBuildInputs = [
    pkg-config
    makeBinaryWrapper
  ]
  ++ lib.optional withWayland wayland-scanner;

  buildInputs = [
    libx11
    libxext
    libxtst
    libxfixes
    gtk3
  ]
  ++ lib.optionals withWayland [
    wayland
    libxkbcommon
  ]
  ++ lib.optionals withPortal [
    glib
    pipewire
    libei
  ]
  ++ lib.optional withViewer SDL2
  ++ lib.optionals withInprocAv [
    libpulseaudio
    libopus
    libgbm
    libxcb
  ];

  # Viewer audio tests decode through the ffmpeg CLI.
  # PTY tests start the shell in $HOME and file-transfer tests write there; the sandbox HOME does not exist.
  nativeCheckInputs = [
    writableTmpDirAsHomeHook
  ]
  ++ lib.optional withViewer ffmpeg-full;

  postInstall = ''
    install -Dm644 packaging/common/systemd/uuremote-native.service.in \
      $out/lib/systemd/user/uuremote-native.service
    substituteInPlace $out/lib/systemd/user/uuremote-native.service \
      --replace-fail '@EXEC_START@' "$out/bin/uuremote-server"

    install -Dm644 packaging/common/desktop/uu-remote-linux.desktop -t $out/share/applications
  ''
  + lib.optionalString withViewer ''
    install -Dm644 packaging/common/desktop/uu-remote-linux-viewer.desktop -t $out/share/applications
  ''
  + ''
    install -Dm644 packaging/common/desktop/uu-remote-linux.svg -t $out/share/icons/hicolor/scalable/apps

    # Opt-in (see the NixOS module): lets the active seat user inject input through /dev/uinput.
    install -Dm644 packaging/common/udev/60-uu-remote-linux-uinput.rules -t $out/lib/udev/rules.d
  '';

  # Libraries the engines dlopen at runtime; added after patchelf shrinks the RUNPATH.
  # Keep the unwrapped host binary named `uuremote-server` in libexec so `/proc/<pid>/exe`
  # still matches what `doctor`, `show`, and `set-encoder`/`set-codec` expect.
  postFixup = ''
    patchelf --add-rpath ${lib.makeLibraryPath finalAttrs.passthru.dlopenLibs} $out/bin/uuremote-server
    mkdir -p $out/libexec/uu-remote-linux
    mv $out/bin/uuremote-server $out/libexec/uu-remote-linux/uuremote-server
    makeWrapper $out/libexec/uu-remote-linux/uuremote-server $out/bin/uuremote-server \
      --suffix PATH : ${lib.makeBinPath [ ffmpeg-full ]}
  ''
  + lib.optionalString withViewer ''
    patchelf --add-rpath ${lib.makeLibraryPath finalAttrs.passthru.dlopenLibs} $out/bin/uuremote-viewer
    wrapProgram $out/bin/uuremote-viewer --suffix PATH : ${lib.makeBinPath [ ffmpeg-full ]}
  '';

  passthru = {
    cargoToml = lib.importTOML ../../Cargo.toml;
    dlopenLibs = [
      linux-pam
      libpulseaudio
      libopus
      libgbm
      libayatana-appindicator
    ]
    ++ lib.optional withInprocAv libva;
  };

  meta = {
    inherit (finalAttrs.passthru.cargoToml.package) description;
    homepage = finalAttrs.passthru.cargoToml.package.repository;
    license = lib.licenses.gpl3Plus;
    mainProgram = "uuremote-server";
    platforms = lib.platforms.linux;
  };
})
