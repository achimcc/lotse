{
  description = "Queue, admit and retry the heavy runs of parallel sessions on one workstation";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  # The RustSec advisory database, pinned like any other input. The `audit`
  # check reads it offline; `nix flake update advisory-db` brings news in.
  inputs.advisory-db = {
    url = "github:rustsec/advisory-db";
    flake = false;
  };

  outputs =
    {
      self,
      nixpkgs,
      advisory-db,
    }:
    let
      # /proc and flock(2): Linux only.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (s: f nixpkgs.legacyPackages.${s});
    in
    {
      packages = forAll (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "lotse";
          # Read out of Cargo.toml so the store path and the crate cannot disagree.
          version = (nixpkgs.lib.importTOML ./Cargo.toml).package.version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          # The integration tests start real processes: sh, sleep, true.
          nativeCheckInputs = [
            pkgs.bash
            pkgs.coreutils
          ];
          meta = {
            description = "Queue, admit and retry the heavy runs of parallel sessions on one workstation";
            homepage = "https://github.com/achimcc/lotse";
            license = pkgs.lib.licenses.agpl3Only;
            mainProgram = "lotse";
            platforms = pkgs.lib.platforms.linux;
          };
        };
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
          ];
        };
      });

      checks = forAll (
        pkgs:
        let
          package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        in
        {
          inherit package;
          # Known advisories against Cargo.lock, read offline from the pinned
          # database.
          audit = pkgs.runCommand "lotse-audit" { nativeBuildInputs = [ pkgs.cargo-audit ]; } ''
            HOME=$TMPDIR cargo-audit audit --no-fetch --db ${advisory-db} --file ${./Cargo.lock}
            touch $out
          '';
          # Bans, sources and licenses of the dependency tree (deny.toml).
          # Inside the package's build environment: the vendored crates are
          # what `cargo metadata` reads there, so nothing is fetched.
          deny = package.overrideAttrs (old: {
            pname = "lotse-deny";
            nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.cargo-deny ];
            buildPhase = "cargo deny --offline check bans sources licenses";
            doCheck = false;
            installPhase = "touch $out";
          });
          clippy = package.overrideAttrs (old: {
            pname = "lotse-clippy";
            nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
            buildPhase = "cargo clippy --all-targets -- -D warnings";
            doCheck = false;
            installPhase = "touch $out";
          });
          fmt = package.overrideAttrs (old: {
            pname = "lotse-fmt";
            nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.rustfmt ];
            buildPhase = "cargo fmt --check";
            doCheck = false;
            installPhase = "touch $out";
          });
        }
      );
    };
}
