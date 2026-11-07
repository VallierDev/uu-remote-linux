import { i18n } from '@kit.LocalizationKit';

export const HOST_LANGUAGE: string = i18n.System.getSystemLanguage().toLowerCase().startsWith('zh') ? 'zh' : 'en';

export function text(chinese: string, english: string): string {
  return HOST_LANGUAGE === 'zh' ? chinese : english;
}
