#!/usr/bin/env python3
"""Check exported payout templates offline using real bundled JS/CSS and CSP.

Generate fixtures with the payout_ui_fixtures Rust example, then run with a
Python Playwright environment. No account credentials or funded entries needed.
All requests outside the local fixture server, and every mutation, are blocked.
"""
import argparse
import asyncio
import functools
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import threading
from urllib.parse import urlsplit

from playwright.async_api import async_playwright


async def audit(options, origin):
    checks = []
    options.output.mkdir(parents=True, exist_ok=True, mode=0o700)
    async with async_playwright() as playwright:
        for engine in options.browsers.split(','):
            browser = await getattr(playwright, engine).launch()
            for width in (320, 390, 1280):
                for theme in ('light', 'dark'):
                    case = f'{engine}-{width}-{theme}'
                    context = await browser.new_context(viewport={'width': width, 'height': 844}, color_scheme=theme)
                    await context.add_init_script("document.addEventListener('securitypolicyviolation', e => { (window.__qaCsp ||= []).push(e.effectiveDirective); });")
                    blocked, errors = [], []
                    async def guard(route):
                        request = route.request
                        if request.method not in ('GET', 'HEAD') or not request.url.startswith(origin + '/'):
                            blocked.append({'method': request.method, 'path': urlsplit(request.url).path})
                            await route.abort()
                        else:
                            await route.continue_()
                    await context.route('**/*', guard)
                    page = await context.new_page()
                    page.on('pageerror', lambda _: errors.append('pageerror'))
                    def check(name, passed, evidence=None):
                        checks.append({'case': case, 'name': name, 'passed': bool(passed), 'evidence': evidence})
                    async def fits():
                        return await page.evaluate('document.documentElement.scrollWidth <= innerWidth + 1')
                    await page.goto(origin + '/payouts.html', wait_until='networkidle')
                    check('populated payout page fits viewport', await fits())
                    check('all payout statuses render', await page.locator('#payouts tbody tr').count() == 7)
                    check('only eligible rows offer invoice fallback', await page.locator('[data-payout-action="invoice"]').count() == 4)
                    for index in (3, 4, 6):
                        check(f'paid/on-chain row {index} has no payout action', await page.locator('#payouts tbody tr').nth(index).locator('[data-payout-action]').count() == 0)
                    trigger = page.locator('[data-payout-action="invoice"]').first
                    check('address and invoice action do not overlap', await trigger.evaluate("e => { const p = e.parentElement.querySelector('p'); return p && e.getBoundingClientRect().top >= p.getBoundingClientRect().bottom; }"))
                    await trigger.focus()
                    await trigger.click()
                    check('invoice dialog has accessible name', await page.get_by_role('dialog', name='Submit Lightning Invoice').count() == 1)
                    check('invoice receives focus', await page.locator('#lightningInvoice').evaluate('(e) => e === document.activeElement'))
                    summary = page.locator('#payoutAmountSummary')
                    check('dialog states exact payout amount', await summary.count() == 1 and '1,000 sats' in await summary.inner_text())
                    check('escrow payout hides legacy consent', not await page.locator('#legacyPayoutWarning').is_visible())
                    await page.locator('#submitPayoutInvoice').click()
                    check('empty invoice gets actionable error', await page.locator('#payoutModalError').is_visible() and 'Please enter' in await page.locator('#payoutModalError').inner_text())
                    check('invoice error announced', await page.locator('#payoutModalError').get_attribute('role') == 'alert')
                    check('dialog fits viewport', await fits())
                    await page.keyboard.press('Escape')
                    check('dialog closes and returns focus', not await page.locator('#payoutModal').is_visible() and await trigger.evaluate('(e) => e === document.activeElement'))
                    legacy = page.locator('[data-legacy="true"]')
                    await legacy.click()
                    check('legacy recovery requires explicit consent', await page.locator('#legacyPayoutWarning').is_visible() and not await page.locator('#legacyPayoutApproved').is_checked())
                    await page.locator('#legacyPayoutApproved').check()
                    await page.locator('#cancelPayoutModal').click()
                    await legacy.click()
                    check('legacy consent resets for each opening', not await page.locator('#legacyPayoutApproved').is_checked())
                    await page.keyboard.press('Escape')
                    await page.locator('[data-payout-action="edit-address"]').click()
                    check('address editor is labelled', await page.locator('#lightningAddressForm').get_by_label('Lightning Address', exact=True).count() == 1)
                    check('address editor receives focus', await page.locator('#payoutLightningAddress').evaluate('(e) => e === document.activeElement'))
                    await page.locator('#payoutLightningAddress').fill('not-an-address')
                    await page.locator('#saveLightningAddress').click()
                    check('invalid address rejected locally', bool(await page.locator('#lightningAddressError').inner_text()))
                    check('address error announced', await page.locator('#lightningAddressError').get_attribute('role') == 'alert')
                    check('address editor fits viewport', await fits())
                    # Sample site diagnostics before screenshot: WebKit Playwright
                    # capture itself injects a blocked inline animation-sync style.
                    check('no application CSP violations', not await page.evaluate('window.__qaCsp?.length || 0'))
                    check('no application runtime errors', not errors)
                    await page.screenshot(path=str(options.output / f'{case}.png'), full_page=True)
                    await page.goto(origin + '/empty.html', wait_until='networkidle')
                    check('empty state explains winnings and returns', 'return' in (await page.locator('#noPayoutsMessage').inner_text()).lower())
                    check('empty page fits viewport', await fits())
                    check('no requests to payment or live services', not blocked, blocked)
                    await context.close()
                    print(case, flush=True)
            await browser.close()
    report = {'scope': 'Offline synthetic rows rendered by current Rust templates, compiled assets and production CSP. No payment or wallet submission.', 'checks': checks}
    report['passed'] = sum(check['passed'] for check in checks)
    report['failed'] = len(checks) - report['passed']
    (options.output / 'report.json').write_text(json.dumps(report, indent=2))
    print(json.dumps({key: report[key] for key in ('passed', 'failed')}))
    return bool(report['failed'])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fixtures', required=True, type=Path)
    parser.add_argument('--output', type=Path, default=Path('/tmp/payout-ui-browser'))
    parser.add_argument('--browsers', default='chromium,firefox,webkit')
    options = parser.parse_args()
    policy = (options.fixtures / 'csp.txt').read_text()
    class Handler(SimpleHTTPRequestHandler):
        def end_headers(self):
            self.send_header('Content-Security-Policy', policy)
            super().end_headers()
        def log_message(self, *args):
            pass
    server = ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(Handler, directory=str(options.fixtures)))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        return asyncio.run(audit(options, f'http://127.0.0.1:{server.server_port}'))
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == '__main__':
    raise SystemExit(main())
