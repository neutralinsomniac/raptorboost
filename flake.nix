{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs =
    {
      nixpkgs,
      ...
    }:
    let
      pkgs = import nixpkgs {
        system = "x86_64-linux";
      };
      lib = pkgs.lib;
    in
    {
      packages.x86_64-linux.default = pkgs.rustPlatform.buildRustPackage {
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
    };
}
