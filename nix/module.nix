# services.hob: runs the house's backend. Every address, place, album,
# camera and secret comes from here; the program holds nothing of any one
# house.
#
# Secrets (the Immich key, CalDAV accounts) are read from files through
# systemd credentials, so they never reach the Nix store or the
# environment. Fitted photos are kept in the service's cache directory.
#
# The module listens and hardens the service; which networks may reach the
# port is the host's to decide (a firewall rule, systemd's IPAddressAllow),
# as is any TLS proxy in front. The MCP endpoint answers loopback only.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib) mkOption types;
  cfg = config.services.hob;
  nullable = t: mkOption {
    type = types.nullOr t;
    default = null;
  };
  camera = types.submodule {
    options = {
      id = mkOption {
        type = types.str;
        description = "The camera's name in Frigate and go2rtc.";
      };
      name = mkOption {
        type = types.str;
        description = "The label screens show.";
      };
      turns = mkOption {
        type = types.bool;
        default = false;
        description = "Whether Frigate can pan and tilt it (ONVIF).";
      };
    };
  };
in
{
  options.services.hob = {
    enable = lib.mkEnableOption "hob, the house's backend";
    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "hob.packages.\${system}.default";
    };
    address = mkOption {
      type = types.str;
      default = "127.0.0.1";
      description = "Address to listen on; 0.0.0.0 for screens on the LAN.";
    };
    port = mkOption {
      type = types.port;
      default = 8970;
    };
    timeZone = mkOption {
      type = types.str;
      default = config.time.timeZone;
      defaultText = lib.literalExpression "config.time.timeZone";
      description = "The house's time zone, for days and times.";
    };
    publicUrl = nullable types.str // {
      description = "Where MCP hosts reach this server, e.g. https://home.example.net; the cards load pictures from it.";
    };
    photos = nullable (
      types.submodule {
        options = {
          immichUrl = mkOption {
            type = types.str;
            example = "http://127.0.0.1:2283";
          };
          apiKeyFile = mkOption {
            type = types.str;
            description = "An Immich API key with album.read, asset.read and asset.view.";
          };
          album = mkOption {
            type = types.str;
            description = "The album the house shows.";
          };
        };
      }
    );
    weather = nullable (
      types.submodule {
        options = {
          latitude = mkOption { type = types.float; };
          longitude = mkOption { type = types.float; };
        };
      }
    );
    air.address = nullable types.str // {
      description = "Address of an AirGradient monitor (local API enabled).";
    };
    calendar.accountsFile = nullable types.str // {
      description = ''CalDAV accounts as JSON: [{"name","url","username","password"}].'';
    };
    cameras = nullable (
      types.submodule {
        options = {
          list = mkOption { type = types.listOf camera; };
          go2rtcUrl = mkOption {
            type = types.str;
            example = "http://127.0.0.1:1984";
          };
          frigateUrl = mkOption {
            type = types.str;
            description = "Frigate's unauthenticated internal API.";
            example = "http://127.0.0.1:5000";
          };
        };
      }
    );
  };

  config = lib.mkIf cfg.enable {
    systemd.services.hob = {
      description = "The house's backend";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      environment = {
        HOB_LISTEN = "${cfg.address}:${toString cfg.port}";
        HOB_TZ = cfg.timeZone;
      }
      // lib.optionalAttrs (cfg.publicUrl != null) { HOB_PUBLIC = cfg.publicUrl; }
      // lib.optionalAttrs (cfg.photos != null) {
        HOB_IMMICH = cfg.photos.immichUrl;
        HOB_IMMICH_KEY_FILE = "%d/immich-key";
        HOB_ALBUM = cfg.photos.album;
      }
      // lib.optionalAttrs (cfg.weather != null) {
        HOB_WEATHER = "${toString cfg.weather.latitude},${toString cfg.weather.longitude}";
      }
      // lib.optionalAttrs (cfg.air.address != null) { HOB_AIR = cfg.air.address; }
      // lib.optionalAttrs (cfg.calendar.accountsFile != null) { HOB_CALDAV_FILE = "%d/caldav"; }
      // lib.optionalAttrs (cfg.cameras != null) {
        HOB_CAMERAS = builtins.toJSON cfg.cameras.list;
        HOB_GO2RTC = cfg.cameras.go2rtcUrl;
        HOB_FRIGATE = cfg.cameras.frigateUrl;
      };
      serviceConfig = {
        ExecStart = lib.getExe cfg.package;
        DynamicUser = true;
        # Photos shrunk to the screen, kept across restarts.
        CacheDirectory = "hob";
        LoadCredential =
          lib.optional (cfg.photos != null) "immich-key:${cfg.photos.apiKeyFile}"
          ++ lib.optional (cfg.calendar.accountsFile != null) "caldav:${cfg.calendar.accountsFile}";
        Restart = "on-failure";
        CapabilityBoundingSet = "";
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        NoNewPrivileges = true;
        PrivateDevices = true;
        PrivateTmp = true;
        ProtectClock = true;
        ProtectControlGroups = true;
        ProtectHome = true;
        ProtectHostname = true;
        ProtectKernelLogs = true;
        ProtectKernelModules = true;
        ProtectKernelTunables = true;
        ProtectProc = "invisible";
        ProtectSystem = "strict";
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
        ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
        ];
      };
    };
  };
}
