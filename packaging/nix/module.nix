{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.uu-remote-linux;
  envValueType =
    with lib.types;
    nullOr (oneOf [
      str
      path
      package
    ]);
in
{
  options.services.uu-remote-linux = {
    enable = lib.mkEnableOption "the unofficial NetEase UU Remote host (systemd user service uuremote-native)";

    package = lib.mkPackageOption pkgs "uu-remote-linux" { };

    uinput.enable = lib.mkEnableOption ''
      /dev/uinput keyboard and mouse injection for the active seat user (needed on most Wayland sessions).
      This permission can inject input into the whole seat, including lock and greeter screens
    '';

    settings = lib.mkOption {
      type = lib.types.submodule {
        freeformType = lib.types.attrsOf envValueType;
        options = {
          UU_FFMPEG_ROOT = lib.mkOption {
            type = envValueType;
            default = null;
            description = ''
              Optional FFmpeg prefix passed to {command}`uuremote-server` (must provide x11grab, libx264, and libx265).
              Leave null to use the FFmpeg provided via PATH; setting this disables `inproc-av` native encoding.
            '';
          };
        };
      };
      default = { };
      example = lib.literalExpression ''
        {
          UU_DESKTOP_BACKEND = "wayland";
          UU_VIDEO_ENCODER = "vaapi";
          UU_AUDIO = "0";
        }
      '';
      description = ''
        Environment variables passed to the {command}`uuremote-server` systemd user service.
      '';
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        environment.systemPackages = [ cfg.package ];
        systemd.packages = [ cfg.package ];
        systemd.user.services.uuremote-native = {
          wantedBy = [ "graphical-session.target" ];
          # Setting `path` would replace the user manager's PATH (hyprctl, pactl, gsettings, ...).
          environment = lib.filterAttrs (_: value: value != null) cfg.settings;
        };
      }
      (lib.mkIf cfg.uinput.enable {
        hardware.uinput.enable = true;
        services.udev.packages = [ cfg.package ];
      })
    ]
  );
}
