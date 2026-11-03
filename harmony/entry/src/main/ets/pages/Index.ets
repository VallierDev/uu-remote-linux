import { HOST_LANGUAGE, text } from '../common/Strings';
import { common } from '@kit.AbilityKit';
import { display } from '@kit.ArkUI';
import { image } from '@kit.ImageKit';
import { initialize, beginLogin, refreshQr, startCapture, startHost,
  requestInput, stop, getState, getQrPixels, HostState, setAssistanceEnabled, resetAssistanceCode } from 'libentry.so';

const CAPTURE_STARTED = 0;
const INPUT_AUTHORIZED = 2;
const POLL_INTERVAL_MS = 300;

@Entry
@Component
struct Index {
  @State message: string = text('正在初始化…', "Initializing…");
  @State loggedIn: boolean = false;
  @State sharing: boolean = false;
  @State preparing: boolean = false;
  @State inputAllowed: boolean = false;
  @State assistanceDeviceId: string = '';
  @State assistanceCode: string = '';
  @State assistanceEnabled: boolean = false;
  @State showAssistanceCode: boolean = false;
  @State qr: image.PixelMap | undefined = undefined;
  private timer: number = -1;
  private qrVersion: number = 0;
  private displayId: number = 0;
  private screenWidth: number = 0;
  private screenHeight: number = 0;

  aboutToAppear(): void {
    const context = this.getUIContext().getHostContext() as common.UIAbilityContext;
    const screen = display.getDefaultDisplaySync();
    this.displayId = screen.id;
    this.screenWidth = screen.width;
    this.screenHeight = screen.height;
    const code = initialize(context.filesDir + '/uuremote/config.json', this.screenWidth, this.screenHeight, HOST_LANGUAGE);
    if (code !== 0) { this.message = text('初始化失败，请重新打开应用', "Initialization failed. Reopen the app."); return; }
    this.update();
    if (!this.loggedIn) { beginLogin(); }
    this.timer = setInterval(() => this.update(), POLL_INTERVAL_MS);
  }

  aboutToDisappear(): void {
    if (this.timer !== -1) { clearInterval(this.timer); this.timer = -1; }
    this.qr?.release();
    this.qr = undefined;
    this.qrVersion = 0;
  }

  private update(): void {
    const state: HostState = getState();
    this.message = state.message;
    this.loggedIn = state.loggedIn;
    this.inputAllowed = state.inputState === INPUT_AUTHORIZED;
    this.assistanceDeviceId = state.assistanceDeviceId;
    this.assistanceCode = state.assistanceCode;
    this.assistanceEnabled = state.assistanceEnabled;
    if (!this.assistanceEnabled) { this.showAssistanceCode = false; }
    if (this.loggedIn && this.qr) { this.qr.release(); this.qr = undefined; }
    if (!this.loggedIn && state.qrVersion !== this.qrVersion && state.qrEdge > 0) {
      this.qrVersion = state.qrVersion;
      const pixels = getQrPixels();
      const next = image.createPixelMapSync(pixels, {
        size: { width: state.qrEdge, height: state.qrEdge },
        pixelFormat: image.PixelMapFormat.RGBA_8888,
        alphaType: image.AlphaType.OPAQUE
      });
      this.qr?.release();
      this.qr = next;
    }
    if (this.preparing && state.captureState === CAPTURE_STARTED) {
      this.preparing = false;
      const code = startHost();
      if (code === 0) { this.sharing = true; }
      else { stop(); this.message = text('服务启动失败，请重新开始共享', "Could not start the service. Restart sharing."); }
    }
    if ((this.sharing && state.captureState !== CAPTURE_STARTED) ||
      (this.preparing && (state.captureState > CAPTURE_STARTED || state.captureState === -2))) {
      this.endSharing();
    }
  }

  private beginSharing(): void {
    const code = startCapture(this.screenWidth, this.screenHeight, this.displayId);
    if (code === 0) { this.preparing = true; }
    else { this.message = text('无法开始屏幕共享（', "Could not start screen sharing (") + code + text('）', ")"); }
  }

  private endSharing(): void {
    stop();
    this.sharing = false;
    this.preparing = false;
    this.showAssistanceCode = false;
  }

  build() {
    Scroll() {
      Column({ space: 24 }) {
        Row({ space: 12 }) {
          Image($r('app.media.app_icon')).width(64).height(64)
          Column({ space: 6 }) {
            Text(text('UU 远程 · 鸿蒙被控端', "UU Remote · HarmonyOS Host")).fontSize(28).fontWeight(FontWeight.Bold).fontColor('#F0F5FF')
            Text(text('使用其他设备，连接你的鸿蒙桌面', "Connect to your HarmonyOS desktop from another device")).fontSize(15).fontColor('#9DAEC8')
          }.alignItems(HorizontalAlign.Start)
        }
        Divider().color('#2A3C56')
        Text(this.message).fontSize(20).fontColor('#BCEDE5').textAlign(TextAlign.Center)
        if (!this.loggedIn) {
          if (this.qr) {
            Image(this.qr).width(260).height(260).interpolation(ImageInterpolation.None).backgroundColor(Color.White).padding(12)
            Text(text('使用 UU 远程 App 扫码并确认登录', "Scan with the UU Remote app and confirm sign-in")).fontColor('#CDD8E8')
            Button(text('刷新二维码', "Refresh QR code")).onClick(() => { refreshQr(); })
          } else {
            Button(text('扫码登录', "Scan to sign in")).onClick(() => {
              if (beginLogin() !== 0) { refreshQr(); }
            })
          }
        } else {
          Text(this.sharing ? text('共享中：同账号可从设备列表连接，其他账号可通过远程协助连接', "Sharing: use the device list for this account, or assistance for another account") : text('账号已登录', "Signed in")).fontColor('#CDD8E8')
          if (!this.sharing && !this.preparing) {
            Button(text('开始共享屏幕', "Start screen sharing")).backgroundColor('#36BFA7').fontColor('#061B21').onClick(() => this.beginSharing())
          } else {
            Button(text('停止共享', "Stop sharing")).backgroundColor('#AE4258').onClick(() => this.endSharing())
          }
          if (this.sharing && this.assistanceDeviceId.length > 0) {
            Column({ space: 12 }) {
              Text(text('远程协助', "Remote assistance")).fontSize(20).fontColor('#F0F5FF')
              Text(text('设备 ID：', "Device ID: ") + this.assistanceDeviceId).fontSize(24).fontColor('#CDD8E8').copyOption(CopyOptions.LocalDevice)
              Row({ space: 12 }) {
                Text(text('允许他人通过设备 ID 连接', "Allow connections by device ID")).fontColor('#CDD8E8')
                Toggle({ type: ToggleType.Switch, isOn: this.assistanceEnabled })
                  .onChange((enabled: boolean) => {
                    if (enabled === this.assistanceEnabled) return;
                    if (setAssistanceEnabled(enabled) !== 0) { this.message = text('协助设置未完成，请稍后重试', "Assistance settings could not be changed. Try again."); }
                  })
              }
              if (this.assistanceEnabled) {
                Row({ space: 12 }) {
                  Text(text('验证码：', "Verification code: ") + (this.showAssistanceCode ? this.assistanceCode : '••••••••'))
                    .fontSize(24).fontColor('#BCEDE5')
                  Button(this.showAssistanceCode ? text('隐藏', "Hide") : text('显示', "Show")).onClick(() => {
                    this.showAssistanceCode = !this.showAssistanceCode;
                  })
                  Button(text('刷新验证码', "Refresh code")).onClick(() => { resetAssistanceCode(); })
                }
                Text(text('对方在 UU 远程的“开始协助”中输入设备 ID 和验证码即可连接。', "The other person can enter this device ID and code under “Start assistance” in UU Remote."))
                  .fontSize(14).fontColor('#9DAEC8')
              }
            }.padding(20).backgroundColor('#18253A').borderRadius(12)
          } else {
            Text(text('开始共享后可获取设备 ID 和验证码，用于其他账号的远程协助。', "Start sharing to get a device ID and code for assistance from another account."))
              .fontSize(14).fontColor('#9DAEC8')
          }
          Button(this.inputAllowed ? text('已允许远程键鼠控制', "Remote keyboard and mouse allowed") : text('允许远程键鼠控制', "Allow remote keyboard and mouse"))
            .enabled(!this.inputAllowed).onClick(() => {
              const code = requestInput();
              if (code !== 0) { this.message = text('授权请求未完成（', "Authorization request failed (") + code + text('）', ")"); }
            })
        }
        Blank()
        Text(text('非官方开源客户端 · 与网易无关', "Unofficial open-source client · Not affiliated with NetEase"))
          .fontSize(13).fontColor('#8295B4').textAlign(TextAlign.Center)
      }.padding(40).width('100%')
    }.width('100%').height('100%').backgroundColor('#10182A').align(Alignment.Top)
  }
}
