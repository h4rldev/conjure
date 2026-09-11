{
  description = "Conjure, the modern build-tool for C and C++.";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
  };

  outputs = {
    self,
    nixpkgs,
    ...
  }: let
    systems = [
      "x86_64-linux"
      "aarch64-linux"
      "x86_64-darwin"
      "aarch64-darwin"
    ];

    eachSystem = nixpkgs.lib.genAttrs systems;

    cargo = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    version = cargo.package.version;

    mkConjure = pkgs: buildType:
      pkgs.rustPlatform.buildRustPackage {
        pname = "conjure-${buildType}";
        inherit version;

        src = ./.;
        cargoLock.lockFile = ./Cargo.lock;
        inherit buildType;

        nativeBuildInputs = [pkgs.pkg-config];
        buildInputs = [pkgs.openssl];
      };
  in {
    packages = eachSystem (system: let
      pkgs = import nixpkgs {inherit system;};
    in {
      conjure-debug = mkConjure pkgs "debug";
      conjure-release = mkConjure pkgs "release";
      default = mkConjure pkgs "release";
    });

    devShells = eachSystem (system: let
      pkgs = import nixpkgs {inherit system;};
    in {
      default = pkgs.mkShell {
        name = "conjure";

        buildInputs = with pkgs; [
          pkg-config
          openssl
          cmake

          ### For the test project.
          mold
          libuv
          openssl.dev
          zlib
        ];
      };
    });
  };
}
