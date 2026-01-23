{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        # Import nixpkgs for the specific system
        pkgs = import nixpkgs { inherit system; };
        lib = pkgs.lib;
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          name = "raptorboost";

          PROTOC = "${pkgs.protobuf}/bin/protoc";

          src = lib.cleanSource ./.;

          cargoLock.lockFile = ./Cargo.lock;

          meta = {
            description = "A remote file/directory transfer tool with a simple interface";
            homepage = "https://github.com/neutralinsomniac/raptorboost";
            license = lib.licenses.mit;
            maintainers = [ ];
          };
        };
      }
    );
}
