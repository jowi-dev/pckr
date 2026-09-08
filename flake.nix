{
  description = "pckr - a tmux session picker";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "aarch64-darwin" "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "pckr";
            version = cargoToml.package.version;
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            # Integration tests shell out to real tmux/git binaries and set up
            # scratch tmux sessions and git repos; neither tmux nor a
            # writable/networked git environment is available inside the nix
            # build sandbox, so tests must be run via `nix develop` instead.
            doCheck = false;
          };
        });

      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              rust-analyzer
              tmux
              git
            ];
          };
        });
    };
}
