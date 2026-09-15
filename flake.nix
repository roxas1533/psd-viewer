{
  description = "Read-only PSD viewer (egui/glow, Wayland)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # winit / glutin / rfd は実行時に dlopen するので、ビルド時のリンクではなく実行時のパスで渡す
      runtimeLibs =
        pkgs: with pkgs; [
          wayland
          libxkbcommon
          libGL
          dbus
        ];

      font = pkgs: "${pkgs.ipaexfont}/share/fonts/truetype/ipaexg.ttf";
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "psd-viewer";
          version = "0.1.0";

          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
              ./data
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          buildType = "dist";

          nativeBuildInputs = [ pkgs.makeWrapper ];

          postInstall = ''
            install -Dm644 data/psd-viewer.desktop $out/share/applications/psd-viewer.desktop
          '';

          postFixup = ''
            patchelf --add-rpath ${pkgs.lib.makeLibraryPath (runtimeLibs pkgs)} $out/bin/psd-viewer
            wrapProgram $out/bin/psd-viewer --set-default PSD_VIEWER_FONT ${font pkgs}
          '';

          meta = {
            mainProgram = "psd-viewer";
            license = pkgs.lib.licenses.mit;
          };
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
          ];

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (runtimeLibs pkgs);
          PSD_VIEWER_FONT = font pkgs;
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
