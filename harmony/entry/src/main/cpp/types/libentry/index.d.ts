export interface HostState {
  message: string;
  loggedIn: boolean;
  qrVersion: number;
  qrEdge: number;
  captureState: number;
  inputState: number;
  frames: number;
  assistanceDeviceId: string;
  assistanceCode: string;
  assistanceEnabled: boolean;
}
export const initialize: (configPath: string, width: number, height: number, language: string) => number;
export const beginLogin: () => number;
export const refreshQr: () => number;
export const startHost: () => number;
export const startCapture: (width: number, height: number, displayId: number) => number;
export const stop: () => number;
export const requestInput: () => number;
export const getState: () => HostState;
export const getQrPixels: () => ArrayBuffer;

export const setAssistanceEnabled: (enabled: boolean) => number;
export const resetAssistanceCode: () => number;
