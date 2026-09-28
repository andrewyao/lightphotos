async (page) => {
  const N = 600;
  const setup = await page.evaluate(async (N) => {
    const listing = await (await fetch('/bench/')).text();
    const names = [...listing.matchAll(/href="([^"]+\.jpg)"/g)].map(m => decodeURIComponent(m[1])).slice(0, N);
    const opfs = await navigator.storage.getDirectory();
    try { await opfs.removeEntry('bench', { recursive: true }); } catch (e) {}
    const root = await opfs.getDirectoryHandle('bench', { create: true });
    const side = await root.getDirectoryHandle('.lightphotos', { create: true });
    const edit = JSON.stringify({ adjustments: { exposure: 0.5, contrast: 20 } });
    let i = 0;
    await Promise.all(Array.from({ length: 8 }, async () => {
      while (i < names.length) {
        const name = names[i++];
        const bytes = await (await fetch('/bench/' + encodeURIComponent(name))).arrayBuffer();
        const w = await (await root.getFileHandle(name, { create: true })).createWritable();
        await w.write(bytes); await w.close();
        const s = await (await side.getFileHandle(name + '.xmp', { create: true })).createWritable();
        await s.write(edit); await s.close();
      }
    }));
    window.__lpTestRoot = root;
    window.__frames = []; window.__long = [];
    new PerformanceObserver(l => { for (const e of l.getEntries()) window.__long.push(e.duration); })
      .observe({ type: 'longtask', buffered: false });
    let last = performance.now();
    const tick = (t) => { window.__frames.push(t - last); last = t; requestAnimationFrame(tick); };
    requestAnimationFrame(tick);
    return names.length;
  }, N);

  const canvas = page.locator('canvas');
  await canvas.click({ position: { x: 800, y: 900 } });
  await page.keyboard.press('Meta+O');
  await page.waitForTimeout(8000);

  const stats = () => page.evaluate(() => {
    const f = window.__frames.slice().sort((a, b) => a - b);
    const q = p => f.length ? +f[Math.min(f.length - 1, Math.floor(p * f.length))].toFixed(1) : null;
    const long = window.__long;
    return {
      frames: f.length, p50: q(0.5), p95: q(0.95), max: q(1),
      over50ms: f.filter(x => x > 50).length,
      longtasks: long.length, longtaskMs: Math.round(long.reduce((a, b) => a + b, 0)),
    };
  });
  const open = await stats();

  await page.evaluate(() => { window.__frames = []; window.__long = []; });
  await page.mouse.move(800, 900);
  const t0 = Date.now();
  for (let k = 0; k < 40; k++) {
    await page.mouse.wheel(0, 400);
    await page.waitForTimeout(50);
  }
  await page.waitForTimeout(4000);
  const scroll = await stats();
  return { photos: setup, open, scroll, scrollWallMs: Date.now() - t0 };
}
