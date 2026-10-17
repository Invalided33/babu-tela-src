'use strict';
const { contextBridge, ipcRenderer } = require('electron');

const boot = ipcRenderer.sendSync('app:boot');
const on = channel => cb => ipcRenderer.on(channel, (e, payload) => cb(payload));

// The renderer gets this one narrow object and nothing else from Node or Electron.
contextBridge.exposeInMainWorld('prosaNative', {
  serverUrl: boot.serverUrl,
  version: boot.version,
  window: {
    minimize: () => ipcRenderer.send('window:minimize'),
    maximize: () => ipcRenderer.send('window:maximize'),
    close: () => ipcRenderer.send('window:close'),
  },
  settings: {
    get: () => ipcRenderer.invoke('settings:get'),
    set: patch => ipcRenderer.invoke('settings:set', patch),
  },
  setBadge: n => ipcRenderer.send('badge:set', n),
  notify: n => ipcRenderer.send('notify:show', n),
  pickScreen: id => ipcRenderer.send('screen:picked', id),
  hostStatus: () => ipcRenderer.invoke('host:status'),
  session: {
    get: () => ipcRenderer.invoke('session:get'),
    set: token => ipcRenderer.invoke('session:set', token),
  },
  checkForUpdates: () => ipcRenderer.invoke('update:check'),
  quitAndInstall: () => ipcRenderer.send('update:install'),
  onTray: on('tray:action'),
  onNavigate: on('navigate'),
  onScreenSources: on('screen:sources'),
  onUpdate: on('update:status'),
});
