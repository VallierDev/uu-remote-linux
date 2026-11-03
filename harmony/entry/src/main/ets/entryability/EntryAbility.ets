import { UIAbility } from '@kit.AbilityKit';
import { window } from '@kit.ArkUI';
import { BusinessError } from '@kit.BasicServicesKit';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { stop } from 'libentry.so';

export default class EntryAbility extends UIAbility {
  onWindowStageCreate(stage: window.WindowStage): void {
    stage.loadContent('pages/Index', (error: BusinessError) => {
      if (error.code) {
        hilog.error(0xFF00, 'UUHarmony', 'Load content failed: code=%{public}d', error.code);
      }
    });
  }

  onDestroy(): void {
    stop();
    hilog.info(0xFF00, 'UUHarmony', 'Ability destroyed; sharing stopped');
  }
}
