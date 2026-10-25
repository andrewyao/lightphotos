async (page) => {
  // The shared wasm memory is created by the page's JS glue, so wrap the
  // constructor before a reload to keep a handle on it.
  await page.addInitScript(() => {
    const Memory = WebAssembly.Memory;
    WebAssembly.Memory = function (d) {
      const m = new Memory(d);
      if (d && d.shared) window.__lpMemory = m;
      return m;
    };
    WebAssembly.Memory.prototype = Memory.prototype;
  });
  await page.reload();
  await page.waitForSelector('canvas');
  // `__lpHeapStats` exists only in a `--features heap-stats` build.
  const heap = () => page.evaluate(() => {
    const mb = b => Math.round(b / 1048576);
    const m = window.__lpMemory;
    const [live, peak] = window.__lpHeapStats ? window.__lpHeapStats() : [null, null];
    return {
      heapMB: m ? mb(m.buffer.byteLength) : null,
      liveMB: live === null ? null : mb(live),
      peakLiveMB: peak === null ? null : mb(peak),
    };
  });

  const photos = await page.evaluate(async () => {
    const listing = await (await fetch('/bench/')).text();
    const names = [...listing.matchAll(/href="([^"]+\.ARW)"/gi)].map(m => decodeURIComponent(m[1])).slice(0, 20);
    const opfs = await navigator.storage.getDirectory();
    try { await opfs.removeEntry('raw', { recursive: true }); } catch (e) {}
    const root = await opfs.getDirectoryHandle('raw', { create: true });
    for (const name of names) {
      const bytes = await (await fetch('/bench/' + encodeURIComponent(name))).arrayBuffer();
      const w = await (await root.getFileHandle(name, { create: true })).createWritable();
      await w.write(bytes); await w.close();
    }
    window.__lpTestRoot = root;
    return names.length;
  });

  const traps = [];
  page.on('pageerror', e => traps.push(e.message));

  const canvas = page.locator('canvas');
  await canvas.click({ position: { x: 800, y: 900 } });
  await page.keyboard.press('Meta+O');
  await page.waitForTimeout(6000);
  await canvas.dblclick({ position: { x: 320, y: 220 } });
  await page.waitForTimeout(3000);
  const heapAfterOpen = await heap();

  const passes = [];
  for (let r = 0; r < 4 && !traps.length; r++) {
    const key = r % 2 ? 'ArrowLeft' : 'ArrowRight';
    for (let k = 0; k < photos - 1 && !traps.length; k++) {
      await page.keyboard.press(key);
      await page.waitForTimeout(60);
    }
    await page.waitForTimeout(3000);
    passes.push({ key, ...await heap() });
  }
  // The heap never shrinks, so the last reading is the peak.
  return { photos, heapAfterOpen, passes, end: await heap(), traps };
}
