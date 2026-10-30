const {chromium}=require(process.env.PLAYWRIGHT_MODULE||'playwright');
const assert=require('node:assert/strict');
(async()=>{
const browser=await chromium.launch({headless:true,args:['--no-sandbox']});
try{const page=await browser.newPage();const errors=[];page.on('pageerror',e=>errors.push(e.message));
const base=process.env.ENDPOINT_URL||'http://localhost:8084';
await page.goto(base+'/');await page.waitForFunction(()=>document.querySelectorAll('tbody tr').length===100);
const before=await page.locator('tbody tr').first().textContent();await page.locator('#next').click();await page.waitForFunction(previous=>document.querySelector('tbody tr').textContent!==previous, before);
await page.locator('[name=airport]').fill('KMIA');await page.locator('form button').click();await page.waitForFunction(()=>!document.querySelector('#status').textContent.includes('Loading'));
assert.ok((await page.locator('#status').textContent()).includes('movements'));
assert.equal((await page.request.get(base+'/api/flights')).status(),410);
await page.setViewportSize({width:390,height:844});assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));assert.deepEqual(errors,[]);
console.log('PASS: endpoint feed, pagination, filtering, retired route, mobile, no page errors');
}finally{await browser.close()}
})().catch(e=>{console.error(e);process.exitCode=1});
