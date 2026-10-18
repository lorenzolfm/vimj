{
  description = "dejay: DJ-style transitions for your own music, from the terminal";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      # What dejay runs at play and download time.
      runtime = pkgs: [ pkgs.ffmpeg pkgs.yt-dlp ];
    in
    {
      packages = forAll (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "dejay";
          version = "0.2.0";
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type: !(pkgs.lib.hasInfix "/target" path);
          };
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.pkg-config pkgs.rustPlatform.bindgenHook pkgs.makeWrapper ];
          buildInputs = [ pkgs.alsa-lib ];
          # The tests need nothing special, but they double the build time.
          doCheck = false;
          postInstall = ''
            for bin in dejay dejayd; do
              wrapProgram $out/bin/$bin --prefix PATH : ${pkgs.lib.makeBinPath (runtime pkgs)}
            done
          '';
          meta.mainProgram = "dejay";
        };
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          nativeBuildInputs = [ pkgs.pkg-config pkgs.rustPlatform.bindgenHook ];
          buildInputs = [ pkgs.alsa-lib ];
          packages = runtime pkgs;
        };
      });
    };
}
