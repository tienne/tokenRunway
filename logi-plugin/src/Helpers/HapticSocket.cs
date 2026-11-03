namespace Loupedeck.TokenRunwayPlugin
{
    using System;
    using System.IO;
    using System.Net.Sockets;
    using System.Text;
    using System.Threading;
    using System.Threading.Tasks;

    // 연결 하나에 이벤트 이름 한 줄을 받는 유닉스 소켓.
    // HTTP 포트로 열면 브라우저에서 아무 페이지나 마우스를 울릴 수 있어서 파일 권한으로 막히는 소켓을 쓴다.
    internal sealed class HapticSocket : IDisposable
    {
        private const Int32 MaxMessageBytes = 64;

        private static readonly TimeSpan ReadTimeout = TimeSpan.FromSeconds(2);

        private readonly String _path;
        private readonly Action<String> _onEvent;
        private readonly CancellationTokenSource _cts = new CancellationTokenSource();
        private Socket _listener;

        public HapticSocket(String path, Action<String> onEvent)
        {
            this._path = path;
            this._onEvent = onEvent;
        }

        public void Start()
        {
            Directory.CreateDirectory(Path.GetDirectoryName(this._path));

            // 서비스가 비정상 종료되면 소켓 파일이 남아 bind가 실패한다
            File.Delete(this._path);

            this._listener = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
            this._listener.Bind(new UnixDomainSocketEndPoint(this._path));
            if (!OperatingSystem.IsWindows())
            {
                File.SetUnixFileMode(this._path, UnixFileMode.UserRead | UnixFileMode.UserWrite);
            }
            this._listener.Listen(8);

            _ = Task.Run(() => this.AcceptLoopAsync(this._cts.Token));
        }

        private async Task AcceptLoopAsync(CancellationToken ct)
        {
            while (!ct.IsCancellationRequested)
            {
                Socket client;
                try
                {
                    client = await this._listener.AcceptAsync(ct);
                }
                catch (Exception ex) when (ex is OperationCanceledException || ex is ObjectDisposedException)
                {
                    return;
                }
                catch (Exception ex)
                {
                    PluginLog.Warning(ex, "Haptic socket accept failed");
                    continue;
                }

                _ = Task.Run(() => this.HandleAsync(client, ct));
            }
        }

        private async Task HandleAsync(Socket client, CancellationToken ct)
        {
            using (client)
            using (var timeout = CancellationTokenSource.CreateLinkedTokenSource(ct))
            {
                // 보내놓고 안 닫는 클라이언트가 있어도 연결이 쌓이지 않게
                timeout.CancelAfter(ReadTimeout);

                var buffer = new Byte[MaxMessageBytes];
                var length = 0;
                try
                {
                    while (length < buffer.Length)
                    {
                        var read = await client.ReceiveAsync(buffer.AsMemory(length), SocketFlags.None, timeout.Token);
                        if (read == 0 || Array.IndexOf(buffer, (Byte)'\n', length, read) >= 0)
                        {
                            length += read;
                            break;
                        }

                        length += read;
                    }
                }
                catch (Exception ex) when (ex is OperationCanceledException || ex is SocketException)
                {
                    return;
                }

                var name = Encoding.UTF8.GetString(buffer, 0, length).Split('\n')[0].Trim();
                if (name.Length > 0)
                {
                    this._onEvent(name);
                }
            }
        }

        public void Dispose()
        {
            this._cts.Cancel();
            this._listener?.Dispose();
            File.Delete(this._path);
            this._cts.Dispose();
        }
    }
}
