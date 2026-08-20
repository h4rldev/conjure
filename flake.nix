{
  description = "Conjure, the modern build-tool for C and C++.";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
  };

  outputs = inputs: let
    system = "x86_64-linux";
    pkgs = import inputs.nixpkgs {inherit system;};
  in {
    devShells.${system}.default = pkgs.mkShell {
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
  };
}
