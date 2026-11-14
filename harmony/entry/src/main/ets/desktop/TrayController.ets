import { text } from '../common/Strings';
import { common } from '@kit.AbilityKit';
import { statusBarManager } from '@kit.DeskTopExtensionKit';
import { image } from '@kit.ImageKit';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { BusinessError } from '@kit.BasicServicesKit';
import emitter from '@ohos.events.emitter';
import { stop } from 'libentry.so';

const LOG_DOMAIN = 0xFF00;
const LOG_TAG = 'UUHarmonyTray';
const MENU_OPEN = 'open';
const MENU_STOP = 'stop';
const MENU_EXIT = 'exit';
const QUICK_OPERATION_HEIGHT = 1;

export function reportTrayError(operation: string, error: BusinessError): void {
  hilog.error(LOG_DOMAIN, LOG_TAG, '%{public}s failed: code=%{public}d', operation, error.code);
}

export class TrayController {
  private context: common.UIAbilityContext;
  private ready: boolean = false;
  private disposed: boolean = false;
  private exiting: boolean = false;
  private whiteIcon: image.PixelMap | undefined = undefined;
  private blackIcon: image.PixelMap | undefined = undefined;
  private openCallback = (): void => {
    this.show().catch((error: BusinessError) => reportTrayError('show', error));
  };
  private menuCallback = (event: emitter.EventData): void => {
    const code: string | undefined = event.data?.menuCode as string | undefined;
    if (code === MENU_OPEN) {
      this.openCallback();
    } else if (code === MENU_STOP) {
      stop();
      hilog.info(LOG_DOMAIN, LOG_TAG, 'Sharing stopped from tray');
    } else if (code === MENU_EXIT) {
      this.exit().catch((error: BusinessError) => reportTrayError('exit', error));
    }
  };

  constructor(context: common.UIAbilityContext) { this.context = context; }

  async install(): Promise<void> {
    try { await this.installTray(); }
    catch (error) {
      this.dispose();
      throw error as Error;
    }
  }

  private async installTray(): Promise<void> {
    this.whiteIcon = await this.loadIcon($r('app.media.tray_white').id);
    this.blackIcon = await this.loadIcon($r('app.media.tray_black').id);
    if (this.disposed) { this.releaseIcons(); return; }
    const item: statusBarManager.StatusBarItem = {
      icons: { white: this.whiteIcon, black: this.blackIcon },
      quickOperation: {
        title: text('UU 远程·鸿蒙', "UU Remote · HarmonyOS"), height: QUICK_OPERATION_HEIGHT,
        abilityName: '', moduleName: 'entry'
      },
      hoverTips: text('UU 远程·鸿蒙 — 点击打开主窗口', "UU Remote · HarmonyOS — Click to open"),
      statusBarGroupMenu: [[
        this.menu(text('打开主窗口', "Open main window"), MENU_OPEN),
        this.menu(text('停止共享', "Stop sharing"), MENU_STOP)
      ], [this.menu(text('退出', "Quit"), MENU_EXIT)]]
    };
    statusBarManager.on('statusBarIconClick', this.openCallback);
    statusBarManager.on('rightMenuClick', this.menuCallback);
    await new Promise<void>((resolve, reject) => {
      statusBarManager.addToStatusBar(this.context, item, (error: BusinessError) => {
        if (error) { reject(error); } else { resolve(); }
      });
    });
    this.ready = true;
    if (this.disposed) { await this.remove(); return; }
    hilog.info(LOG_DOMAIN, LOG_TAG, 'Tray installed');
  }

  isExiting(): boolean { return this.exiting; }

  async hide(): Promise<boolean> {
    if (!this.ready || this.exiting) return false;
    await this.context.hideAbility();
    hilog.info(LOG_DOMAIN, LOG_TAG, 'Window hidden to tray');
    return true;
  }

  async show(): Promise<void> {
    if (!this.ready || this.exiting) return;
    await this.context.showAbility();
    hilog.info(LOG_DOMAIN, LOG_TAG, 'Window restored from tray');
  }

  dispose(): void {
    this.disposed = true;
    statusBarManager.off('statusBarIconClick', this.openCallback);
    statusBarManager.off('rightMenuClick', this.menuCallback);
    this.remove().catch((error: BusinessError) => reportTrayError('remove', error));
  }

  private async exit(): Promise<void> {
    if (this.exiting) return;
    this.exiting = true;
    stop();
    try {
      try { await this.remove(); }
      catch (error) { reportTrayError('remove on exit', error as BusinessError); }
      await this.context.terminateSelf();
    } catch (error) {
      this.exiting = false;
      throw error as Error;
    }
  }

  private async remove(): Promise<void> {
    if (this.ready) {
      await new Promise<void>((resolve, reject) => {
        statusBarManager.removeFromStatusBar(this.context, (error: BusinessError) => {
          if (error) { reject(error); } else { resolve(); }
        });
      });
      this.ready = false;
    }
    this.releaseIcons();
  }

  private releaseIcons(): void {
    this.whiteIcon?.release();
    this.blackIcon?.release();
    this.whiteIcon = undefined;
    this.blackIcon = undefined;
  }

  private async loadIcon(id: number): Promise<image.PixelMap> {
    const bytes = this.context.resourceManager.getMediaContentSync(id);
    const source = image.createImageSource(bytes.buffer);
    try { return await source.createPixelMap(); }
    finally { source.release(); }
  }

  private menu(title: string, code: string): statusBarManager.StatusBarMenuItem {
    return {
      title: title, menuCode: code,
      menuAction: { abilityName: 'EntryAbility', moduleName: 'entry', notifyOnly: true }
    };
  }
}
