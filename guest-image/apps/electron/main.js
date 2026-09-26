// An Electron app for the application pass (apps.sh electron): the WebGL and
// video page in a BrowserWindow, and Electron's own account of its GPU
// process (getGPUFeatureStatus, getGPUInfo) on stdout.
const { app, BrowserWindow } = require('electron');
app.setName('nvgpu-electron');
app.whenReady().then(() => {
  const w = new BrowserWindow({ width: 1280, height: 860, title: 'nvgpu-electron' });
  // (event) with event.message since Electron 35; (event, level, message) before.
  w.webContents.on('console-message', (e, level, message) => {
    const msg = e && e.message !== undefined ? e.message : message;
    if (typeof msg === 'string' && msg.startsWith('NVGPU_PAGE')) console.log(msg);
  });
  w.loadFile('/opt/nvgpu/apps/www/gl.html', { query: { v: 'h264.mp4' } });
  setTimeout(async () => {
    console.log('NVGPU_ELECTRON featureStatus ' + JSON.stringify(app.getGPUFeatureStatus()));
    // 'basic' answers at once; 'complete' waits for a GPU-process info
    // collection that can outlast the slot.
    try {
      const i = await app.getGPUInfo('basic');
      const a = i.auxAttributes || {};
      console.log('NVGPU_ELECTRON gpu ' + JSON.stringify(i.gpuDevice) + ' glVendor=' + a.glVendor +
                  ' glRenderer=' + a.glRenderer + ' glImplementation=' + a.glImplementationParts);
    } catch (err) { console.log('NVGPU_ELECTRON getGPUInfo failed ' + err); }
  }, 10000);
});
