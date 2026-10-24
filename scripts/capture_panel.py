#!/usr/bin/env python3
"""Capture an isolated, already running DRY-RUN panel without trade mutations."""
import argparse
import json
from pathlib import Path
from urllib.parse import urlsplit

from playwright.sync_api import sync_playwright


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:8080")
    parser.add_argument("--token-file", required=True, type=Path)
    parser.add_argument("--output", type=Path,
                        default=Path("/home/gera/.dsh/missions-v31/panel-screens"))
    parser.add_argument("--chrome", default="/usr/bin/google-chrome")
    args = parser.parse_args()
    origin = urlsplit(args.url)
    if origin.scheme != "http" or origin.hostname not in {"127.0.0.1", "localhost"}:
        parser.error("Only a local HTTP panel is supported")
    token = args.token_file.read_text().strip()
    if not token:
        parser.error("Token file is empty")
    args.output.mkdir(parents=True, exist_ok=True)
    evidence = {"url": args.url, "screenshots": [], "networks": [],
                "browser_errors": [], "blocked_requests": []}
    with sync_playwright() as p:
        browser = p.chromium.launch(executable_path=args.chrome, headless=True,
                                    args=["--no-sandbox"])
        context = browser.new_context(viewport={"width": 1440, "height": 1100},
                                      device_scale_factor=1, color_scheme="dark")

        def guard(route):
            request = route.request
            url = urlsplit(request.url)
            local = (url.scheme, url.netloc) == (origin.scheme, origin.netloc)
            local_chat = (url.path == "/api/ai/chat" and request.method == "POST"
                          and request.post_data_json.get("provider") == "local")
            if local and (request.method == "GET" or local_chat):
                route.continue_()
            else:
                evidence["blocked_requests"].append({"method": request.method,
                                                      "path": url.path})
                route.abort()

        context.route("**/*", guard)
        page = context.new_page()
        page.on("pageerror", lambda error: evidence["browser_errors"].append(str(error)))
        page.goto(args.url, wait_until="networkidle")
        status = context.request.get(args.url + "/api/panel/status?network=solana",
                                     headers={"Authorization": "Bearer " + token})
        assert status.ok, "Panel authentication failed"
        runtime = status.json()
        assert runtime.get("live_enabled") is False, "Capture requires DRY-RUN"
        assert not runtime.get("cli_live", False), "Capture requires startup without --real"
        page.locator("#token").fill(token)
        page.locator("#auth button").click()
        page.wait_for_function("document.querySelector('#connection').textContent.includes('Connected locally')")
        assert page.locator("#mode").inner_text() == "DRY RUN"
        evidence["mode"] = "DRY RUN"

        def capture(name, selector=None):
            if selector:
                page.locator(selector).scroll_into_view_if_needed()
                page.locator(selector).screenshot(path=str(args.output / name))
            else:
                page.evaluate("window.scrollTo(0, 0)")
                page.screenshot(path=str(args.output / name))
            evidence["screenshots"].append(name)

        capture("01-dashboard.png")
        page.locator("#message").fill("Summarize the observed activity and current execution mode.")
        with page.expect_response(lambda response: urlsplit(response.url).path == "/api/ai/chat") as reply:
            page.locator("#chat-form button").click()
        assert reply.value.ok, "Local AI request failed"
        page.wait_for_function("!document.querySelector('#chat-output').textContent.includes('Choose local analytics')")
        evidence["local_ai_response"] = True
        capture("02-ai.png", "#intelligence")
        capture("03-strategies.png", "#strategy-engine")
        capture("03-paper-research.png", "#paper-portfolio")
        for network in ["solana", "robinhood", "elysium"]:
            page.locator(f'[data-network="{network}"]').click()
            page.wait_for_function("!busy")
            assert page.locator(f'[data-network="{network}"]').get_attribute("aria-pressed") == "true"
            assert page.locator("#paper-network").inner_text().startswith(network.upper())
            assert page.locator("#mode").inner_text() == "DRY RUN"
            capture(f"04-network-{network}.png")
            capture(f"05-connections-{network}.png", "#connections")
            evidence["networks"].append(network)
        browser.close()
    (args.output / "capture-evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps(evidence, indent=2))
    assert not evidence["browser_errors"], "Panel JavaScript errors occurred"
    assert not evidence["blocked_requests"], "Panel attempted a blocked request"


if __name__ == "__main__":
    main()
