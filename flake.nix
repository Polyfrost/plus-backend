{
    inputs = {
        nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
        flake-utils.url = "github:numtide/flake-utils";
        rust-overlay = {
            url = "github:oxalica/rust-overlay";
            inputs.nixpkgs.follows = "nixpkgs";
        };
        crane.url = "github:ipetkov/crane";
        treefmt-nix = {
            url = "github:numtide/treefmt-nix";
            inputs.nixpkgs.follows = "nixpkgs";
        };
    };

    outputs =
        {
            self,
            nixpkgs,
            flake-utils,
            rust-overlay,
            crane,
            treefmt-nix,
            ...
        }:
        flake-utils.lib.eachDefaultSystem (
            system:
            let
                # Initialize nixpkgs
                pkgs = nixpkgs.legacyPackages.${system};
                inherit (pkgs) lib;
                # Setup the rust toolchain
                rust-bin = rust-overlay.lib.mkRustBin { } pkgs;
                rust' = (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml);
                # Setup rust nix packaging
                craneLib = (crane.mkLib pkgs).overrideToolchain (_: rust');
                stdenvSelector =
                    p: if p.stdenv.hostPlatform.isElf then p.stdenvAdapters.useMoldLinker p.stdenv else p.stdenv;
                commonArgs = {
                    src = craneLib.cleanCargoSource ./.;
                    strictDeps = true;

                    buildInputs = with pkgs; [
                        libpq
                        openssl
                    ];

                    nativeBuildInputs = with pkgs; [ pkg-config ];

                    # Use mold linker for faster builds on ELF platforms
                    stdenv = stdenvSelector;
                };
                cargoArtifacts = craneLib.buildDepsOnly commonArgs;
                commonArgsWithDeps = commonArgs // {
                    inherit cargoArtifacts;
                };
                cranePackage = craneLib.buildPackage (
                    commonArgsWithDeps
                    // {
                        meta = {
                            mainProgram = "plus-backend";
                            license = lib.licenses.gpl3Plus;
                        };
                    }
                );
                # Setup treefmt-nix
                treefmtModule = import ./treefmt.nix { inherit rust'; };
                treefmtEval = treefmt-nix.lib.evalModule pkgs treefmtModule;
            in
            {
                packages = {
                    default = self.packages.${system}.plus-backend;
                    plus-backend = cranePackage;
                };
                formatter = treefmtEval.config.build.wrapper;
                devShells.default =
                    craneLib.devShell.override { mkShell = pkgs.mkShell.override { stdenv = stdenvSelector pkgs; }; }
                        {
                            # Add all build-time dependencies to the environment
                            packages =
                                cranePackage.buildInputs
                                ++ cranePackage.nativeBuildInputs
                                ++ (with pkgs; [
                                    # External rust dev utilities
                                    diesel-cli
                                    cargo-deny
                                    cargo-udeps
                                    cargo-nextest
                                    # Rust repl for testing
                                    evcxr
                                    # Debugging
                                    lldb
                                    # Add treefmt wrapper to the PATH for ease of use
                                    self.formatter.${system}
                                ]);
                        };
            }
        );
}
