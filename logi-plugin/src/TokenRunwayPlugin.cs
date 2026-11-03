namespace Loupedeck.TokenRunwayPlugin
{
    using System;
    using System.IO;

    // Token Runway 앱이 보내는 이벤트를 MX Master 4 진동으로 바꾼다.
    // 진동 모터는 Actions SDK 플러그인 안에서만 열려 있어서 앱이 직접 울릴 수 없다.
    public class TokenRunwayPlugin : Plugin
    {
        // 앱(haptic.rs)이 보내는 이름과 같아야 한다. 진동 패턴은 events/extra/eventMapping.yaml에서 고른다
        private static readonly String[] HapticEvents = { "taskDone", "turnDone", "quotaLow", "etaDanger", "resetSoon" };

        private HapticSocket _socket;

        public override Boolean UsesApplicationApiOnly => true;

        public override Boolean HasNoApplication => true;

        public TokenRunwayPlugin()
        {
            PluginLog.Init(this.Log);
            PluginResources.Init(this.Assembly);
        }

        public override void Load()
        {
            foreach (var name in HapticEvents)
            {
                this.PluginEvents.AddEvent(name, name, null);
            }

            try
            {
                this._socket = new HapticSocket(SocketPath(), this.OnHapticEvent);
                this._socket.Start();
                PluginLog.Info($"Listening on {SocketPath()}");
            }
            catch (Exception ex)
            {
                PluginLog.Error(ex, "Failed to open haptic socket");
                this.OnPluginStatusChanged(Loupedeck.PluginStatus.Error, "Token Runway 소켓을 열지 못했어요. 플러그인을 다시 시작해주세요.");
            }
        }

        public override void Unload() => this._socket?.Dispose();

        // 앱의 settings::app_dir()와 같은 위치 — trw.sock 옆에 둔다
        private static String SocketPath() =>
            Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), ".token-runway", "haptic.sock");

        private void OnHapticEvent(String name)
        {
            if (Array.IndexOf(HapticEvents, name) < 0)
            {
                PluginLog.Warning($"Unknown haptic event: {name}");
                return;
            }

            try
            {
                this.PluginEvents.RaiseEvent(name);
                PluginLog.Info($"Raised haptic event: {name}");
            }
            catch (Exception ex)
            {
                PluginLog.Error(ex, $"Failed to raise haptic event: {name}");
            }
        }
    }
}
