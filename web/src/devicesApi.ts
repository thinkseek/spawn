// Browser API wrappers for `/api/auth/whoami` and Android device relay (`/api/devices*`).

import { apiHeaders, reportUnauthorizedStatus } from "./api";
import {
  fetchDevicesWithFetch,
  fetchWhoamiWithFetch,
  postDeviceCommandWithFetch,
  type DeviceActionName,
  type DeviceRelayCommand,
  type DevicesFetch,
  type DevicesListResponse,
  type DevicesResult,
  type WhoamiInfo,
} from "./devices.ts";

export * from "./devices.ts";

const browserFetch: DevicesFetch = (url, init) => fetch(url, init);
const onUnauthorized = () => reportUnauthorizedStatus(401);

export function fetchWhoami(signal?: AbortSignal): Promise<DevicesResult<WhoamiInfo>> {
  return fetchWhoamiWithFetch(browserFetch, apiHeaders(), { signal, onUnauthorized });
}

export function fetchDevices(signal?: AbortSignal): Promise<DevicesResult<DevicesListResponse>> {
  return fetchDevicesWithFetch(browserFetch, apiHeaders(), { signal, onUnauthorized });
}

export function postDeviceCommand(
  deviceId: string,
  action: DeviceActionName,
  params: Record<string, unknown>,
  signal?: AbortSignal,
): Promise<DevicesResult<DeviceRelayCommand>> {
  return postDeviceCommandWithFetch(browserFetch, apiHeaders(), deviceId, action, params, {
    signal,
    onUnauthorized,
  });
}
