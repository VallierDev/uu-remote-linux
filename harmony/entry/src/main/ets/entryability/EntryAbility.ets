import { UIAbility } from '@kit.AbilityKit';
import { window } from '@kit.ArkUI';
import { BusinessError } from '@kit.BasicServicesKit';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { stop } from 'libentry.so';
import { TrayController, reportTrayError } from '../desktop/TrayController';

export default class EntryAbility extends UIAbility {
  private tray: TrayController | undefined = undefined;
  private mainWindow: window.Window | undefined = undefined;
  private closeCallback = async (): Promise<boolean> => {
    if (this.tray?.isExiting()) return false;
    try {
      if (await this.tray?.hide()) return true;
      // Keep an accessible Dock entry if the status bar is temporarily unavailable.
      await this.mainWindow?.minimize();
      return true;
    } catch (error) {
      reportTrayError('close to tray', error as BusinessError);
      return true;
    }
  };

  onWindowStageCreate(stage: window.WindowStage): void {
    this.tray = new TrayController(this.context);
    this.mainWindow = stage.getMainWindowSync();
    this.mainWindow.on('windowWillClose', this.closeCallback);
    stage.loadContent('pages/Index', (error: BusinessError) => {
      if (error.code) { reportTrayError('load content', error); }
    });
    this.tray.install().catch((error: BusinessError) => reportTrayError('install tray', error));
  }

  onDestroy(): void {
    stop();
    this.tray?.dispose();
    this.tray = undefined;
    hilog.info(0xFF00, 'UUHarmonyTray', 'Ability destroyed; sharing stopped');
  }
}
