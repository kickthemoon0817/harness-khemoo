"""
Renders a snapshot as one self-contained HTML page: inline CSS, inline SVG sparklines, no scripts.
"""
import html
import urllib.parse
from datetime import datetime, timezone
from typing import Any, ClassVar

from dashboard_lib.models import JsonDict
from dashboard_lib.parsers import StateParsers


class HtmlRenderer:
    """
    Turns the collector's snapshot into the operator page.

    The page reloads itself every `refresh_s` seconds through a meta refresh, so it needs no script. Every value
    from the snapshot is HTML-escaped; status always pairs a shape and a word with its colour.
    """

    SPARK_WIDTH: ClassVar[int] = 240
    SPARK_HEIGHT: ClassVar[int] = 34
    SPARK_HIT_BUCKETS: ClassVar[int] = 48
    STATUS_LEVELS: ClassVar[dict[str, str]] = {
        "ok": "good", "held": "info", "alive": "good", "free": "idle", "orphan": "critical", "down": "critical",
        "slow": "warning", "unknown": "idle", "demoted": "serious", "paused": "idle", "running": "good",
    }
    QUEUE_TITLES: ClassVar[dict[str, str]] = {
        "running": "Running", "claimable": "Claimable", "operator": "Waiting on the operator",
        "signoff": "Waiting for sign-off", "other": "Everything else",
    }
    STYLE: ClassVar[str] = """
:root{color-scheme:light;--page:#f9f9f9;--surface:#ffffff;--ink:#191919;--ink2:#666666;--hair:#e7e7e7;
--axis:#c2c2c2;--border:rgba(11,11,11,.10);--series:#257de0;--series-fill:rgba(37,125,224,.13);--track:#e3eefb;
--good:#0aac23;--warning:#fbb315;--serious:#f18a59;--critical:#dc3c3d;--idle:#9a9a9a;--info:#257de0;
--chip:#f1f1f1;--link:#1b64be;--row:#fafafa}
@media (prefers-color-scheme:dark){:root:not([data-theme="light"]){color-scheme:dark;--page:#0b0b0b;
--surface:#191919;--ink:#efefef;--ink2:#9c9c9c;--hair:#303030;--axis:#474747;--border:rgba(255,255,255,.10);
--series:#338ced;--series-fill:rgba(51,140,237,.18);--track:#1d2a3a;--idle:#6f6f6f;--info:#338ced;--chip:#262626;
--link:#6aabf1;--row:#1e1e1e}}
:root[data-theme="dark"]{color-scheme:dark;--page:#0b0b0b;--surface:#191919;--ink:#efefef;--ink2:#9c9c9c;
--hair:#303030;--axis:#474747;--border:rgba(255,255,255,.10);--series:#338ced;--series-fill:rgba(51,140,237,.18);
--track:#1d2a3a;--idle:#6f6f6f;--info:#338ced;--chip:#262626;--link:#6aabf1;--row:#1e1e1e}
*{box-sizing:border-box}
body{margin:0;background:var(--page);color:var(--ink);font:13px/1.35 system-ui,-apple-system,"Segoe UI",sans-serif}
a{color:var(--link);text-decoration:none}a:hover{text-decoration:underline}
main{padding:12px 16px 20px;display:grid;gap:12px;max-width:2200px;margin:0 auto}
header{display:flex;flex-wrap:wrap;align-items:center;gap:8px 18px;padding:10px 16px;background:var(--surface);
border-bottom:1px solid var(--border)}
header h1{font-size:15px;margin:0;font-weight:650}
.sub{color:var(--ink2)}.num{font-variant-numeric:tabular-nums}
.grid{display:grid;gap:12px}
.machines{grid-template-columns:repeat(auto-fit,minmax(320px,1fr))}
.split{grid-template-columns:minmax(0,3fr) minmax(0,2fr)}
.queue{grid-template-columns:repeat(4,minmax(0,1fr))}.stack{display:grid;gap:12px;align-content:start}
@media (max-width:1300px){.queue{grid-template-columns:repeat(2,minmax(0,1fr))}}
@media (max-width:700px){.queue{grid-template-columns:minmax(0,1fr)}}
.thirds{grid-template-columns:repeat(auto-fit,minmax(340px,1fr))}
@media (max-width:1100px){.split{grid-template-columns:minmax(0,1fr)}}
.panel{background:var(--surface);border:1px solid var(--border);border-radius:8px;padding:10px 12px;min-width:0}
.panel h2{font-size:12px;text-transform:uppercase;letter-spacing:.04em;margin:0 0 8px;color:var(--ink2);
font-weight:650;display:flex;gap:8px;align-items:baseline;flex-wrap:wrap}
.panel h2 .sub{text-transform:none;letter-spacing:0;font-weight:400}
.scroll{max-height:380px;overflow:auto}
table{width:100%;border-collapse:collapse;table-layout:fixed}
th{font-weight:600;color:var(--ink2);text-align:left;font-size:11px;padding:3px 6px;border-bottom:1px solid var(--hair)}
td{padding:4px 6px;border-bottom:1px solid var(--hair);vertical-align:top;overflow:hidden;text-overflow:ellipsis;
white-space:nowrap;font-variant-numeric:tabular-nums}
tr.dim td{color:var(--ink2)}
td.wrap{white-space:normal}
.pill{display:inline-flex;align-items:center;gap:5px;padding:1px 8px 1px 6px;border-radius:999px;
background:var(--chip);border:1px solid var(--border);font-size:12px;white-space:nowrap;color:var(--ink)}
.pill i{width:8px;height:8px;border-radius:50%;background:var(--idle);flex:none}
.pill.good i{background:var(--good)}.pill.info i{background:var(--info)}
.pill.warning i{background:var(--warning);border-radius:0;clip-path:polygon(50% 0,100% 100%,0 100%)}
.pill.serious i{background:var(--serious);border-radius:1px;transform:rotate(45deg) scale(.85)}
.pill.critical i{background:var(--critical);border-radius:1px}
.pill.idle i{background:transparent;border:1.5px solid var(--idle)}
.chip{display:inline-block;padding:0 6px;border-radius:4px;background:var(--chip);font-size:11px;color:var(--ink2);
margin-right:3px;white-space:nowrap}
.chip.strong{color:var(--ink);font-weight:600}
.card-head{display:flex;justify-content:space-between;gap:8px;align-items:flex-start;margin-bottom:4px}
.card-head b{font-size:14px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.pills{display:flex;gap:4px;flex-wrap:wrap;justify-content:flex-end}
.meters{display:grid;grid-template-columns:44px minmax(0,1fr) auto;gap:4px 8px;align-items:center;margin:6px 0}
.meters span.k{color:var(--ink2);font-size:12px}
.meter{height:8px;border-radius:4px;background:var(--track);overflow:hidden;position:relative}
.meter b{display:block;height:100%;border-radius:4px;background:var(--series)}
.meter b.warning{background:var(--warning)}.meter b.serious{background:var(--serious)}
.meter b.critical{background:var(--critical)}.meter b.none{width:0}
.meter .tick{position:absolute;top:-2px;bottom:-2px;width:1px;background:var(--ink2);opacity:.55}
.meters em{font-style:normal;font-size:12px;text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}
.spark{display:grid;grid-template-columns:72px minmax(0,1fr) 64px;gap:6px;align-items:center;margin-top:4px}
.spark .k{color:var(--ink2);font-size:11px;line-height:1.15}
.spark .v{font-size:11px;text-align:right;color:var(--ink2);font-variant-numeric:tabular-nums}
.spark svg{width:100%;height:34px;display:block;background:var(--row);border-radius:4px}
.spark .line{fill:none;stroke:var(--series);stroke-width:1.5;vector-effect:non-scaling-stroke;stroke-linejoin:round}
.spark .area{fill:var(--series-fill);stroke:none}
.spark .ref{stroke:var(--axis);stroke-dasharray:3 3;stroke-width:1;vector-effect:non-scaling-stroke}
.spark .down{fill:var(--critical)}
.spark .hit{fill:transparent}.spark .hit:hover{fill:var(--series-fill)}
.kv{display:flex;flex-wrap:wrap;gap:2px 12px;color:var(--ink2);font-size:12px;margin:4px 0}
.kv b{color:var(--ink);font-weight:600}
.list{margin:6px 0 0;padding:0;list-style:none;font-size:12px}
.list li{padding:2px 0;border-top:1px dashed var(--hair);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.usage{display:grid;grid-template-columns:auto 160px auto;gap:2px 8px;align-items:center;font-size:12px}
.flag{display:flex;gap:6px;flex-wrap:wrap;align-items:center}
.err{color:var(--critical);font-size:12px}
.note{color:var(--ink2);font-size:12px}.line{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.clamp{display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;overflow:hidden;white-space:normal}
.empty{color:var(--ink2);font-style:italic;padding:4px 0}
.list li.fold{white-space:normal}
.list details summary{cursor:pointer;color:var(--ink2);list-style:revert}
.list details[open] summary{margin-bottom:2px}
.containers{margin:0;padding:0 0 0 14px;list-style:none}
.containers li{display:flex;justify-content:space-between;gap:8px;border-top:0;padding:1px 0}
.containers li .sub{white-space:nowrap;font-size:11px}
.hscroll{overflow-x:auto}.hscroll table{min-width:560px}
.flow-scroll{overflow:auto;max-width:100%;border-radius:6px;background:var(--row);border:1px solid var(--border)}
.flow-canvas{position:relative;font-size:11px;line-height:1.3}
.flow-edges{position:absolute;left:0;top:0;pointer-events:none;overflow:visible}
.fedge{fill:none;stroke-width:1.5;opacity:.75}
.fedge.card{stroke:var(--series)}.fedge.compile{stroke:var(--serious)}
.fedge.waits{stroke-dasharray:5 4;opacity:.7}
.fdev{position:absolute;background:var(--surface);border:1px solid var(--border);border-radius:8px;padding:6px 9px;
overflow:hidden;display:flex;flex-direction:column;gap:3px}
.fdev .top{display:flex;justify-content:space-between;gap:6px;align-items:center}
.fdev .top b{font-size:13px}.fdev .sub{font-size:11px}
.fdev .meter{height:6px}
.fdev .anchors{position:absolute;left:0;right:0;bottom:2px;height:12px;font-size:10px;color:var(--ink2)}
.fdev .anchors span{position:absolute;transform:translateX(-50%)}
.fdev .holders{white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.fdev.flagged{border-color:var(--warning);box-shadow:0 0 0 1px var(--warning)}
.fcol{position:absolute;font-size:11px;font-weight:650;color:var(--ink2);text-transform:uppercase;letter-spacing:.04em;
border-bottom:1px solid var(--hair);padding:2px 0 3px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;
background:var(--row)}
.fcol .sub{font-weight:400;text-transform:none;letter-spacing:0}
.fmore{position:absolute;font-size:11px;color:var(--ink2);font-style:italic}
.fcard{position:absolute;background:var(--surface);border:1px solid var(--border);border-left:3px solid var(--idle);
border-radius:6px;padding:4px 6px;overflow:hidden}
.fcard .l1{display:flex;gap:4px;align-items:center;white-space:nowrap;overflow:hidden}
.fcard .l1 a{font-weight:650}
.fcard .chip{font-size:10px;padding:0 4px;margin:0}
.fcard .l2,.fcard .l3{white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.fcard .l3{color:var(--ink2)}
.fcard.claimed{border-left-color:var(--info)}.fcard.compiling{border-left-color:var(--serious)}
.fcard.waiting_card{border-left-color:var(--warning)}.fcard.running{border-left-color:var(--good)}
.fcard.landing{border-left-color:var(--info)}.fcard.done{border-left-color:var(--good);opacity:.8}
.fcard.moved{animation:flow-move 1.1s cubic-bezier(.2,.7,.2,1) both}
.fcard.fresh{animation:flow-in .9s ease-out both}
.fedge.fresh{animation:flow-fade .8s ease-out .3s both}
.fedge.late{animation:flow-fade .6s ease-out 1.1s both}
@keyframes flow-move{from{transform:translate(var(--dx),var(--dy))}to{transform:none}}
@keyframes flow-in{from{opacity:0;transform:translateY(-10px) scale(.96)}to{opacity:1;transform:none}}
@keyframes flow-fade{from{opacity:0}}
@media (prefers-reduced-motion:reduce){.fcard,.fedge{animation:none!important}}
.flow-legend{display:flex;gap:14px;flex-wrap:wrap;align-items:center;font-size:11px;color:var(--ink2);margin:0 0 6px}
.flow-legend svg{vertical-align:middle}
.flow-trail{margin:8px 0 0;padding:0;list-style:none;font-size:12px}
.flow-trail li{padding:2px 0;border-top:1px dashed var(--hair);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
footer{color:var(--ink2);font-size:11px}
footer table td,footer table th{font-size:11px;padding:2px 6px}
"""

    def render(self, snapshot: JsonDict, refresh_s: int, access_token: str = "") -> str:
        """
        Renders the whole page.

        Args:
            snapshot: The collector's snapshot.
            refresh_s: Seconds between automatic reloads.
            access_token: The server's access token, carried by the reload and the JSON link.

        Returns:
            The HTML document.
        """
        token_query: str = f"?t={urllib.parse.quote(access_token, safe='')}" if access_token else ""
        body: str = "".join((
            self._header(snapshot),
            "<main>",
            f'<section class="grid machines">{"".join(self._machine_card(machine, snapshot) for machine in snapshot.get("machines", []))}</section>',
            f'<section class="grid split">{self._ticks_panel(snapshot)}{self._leases_panel(snapshot)}</section>',
            self._queue_section(snapshot),
            f'<section class="grid thirds">{self._landings_panel(snapshot)}{self._stalls_panel(snapshot)}'
            f'{self._allocator_panel(snapshot)}</section>',
            self._flow_section(snapshot),
            self._sources_footer(snapshot, token_query),
            "</main>",
        ))
        return (
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">"
            "<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">"
            f"<meta http-equiv=\"refresh\" content=\"{int(refresh_s)};url=/{self._e(token_query)}\">"
            "<meta name=\"referrer\" content=\"no-referrer\">"
            "<link rel=\"icon\" href=\"data:,\"><title>Harness status</title>"
            f"<style>{self.STYLE}</style></head><body>{body}</body></html>"
        )

    def _header(self, snapshot: JsonDict) -> str:
        """
        Renders the top bar: harness state, tick width, work waiting, usage windows and models.

        Args:
            snapshot: The snapshot.

        Returns:
            The header HTML.
        """
        harness: JsonDict = snapshot.get("harness", {})
        usage: JsonDict = snapshot.get("usage", {})
        state_pill: str = self._pill("paused", "paused: " + harness.get("paused_reason", "")) if harness.get("paused") \
            else self._pill("running", "running")
        width_text: str = f"{harness.get('live_ticks', 0)} ticks live"
        if harness.get("last_allowed") is not None:
            width_text += (f" &middot; allocator allowed {harness['last_allowed']} at "
                           f"{self._clock(harness.get('last_allowed_at'))}")
        waiting_pill: str = ""
        from_failing_read: str = " at the last good read" if harness.get("waiting_count_from_failing_read") else ""
        if harness.get("work_waiting"):
            waiting_pill = self._pill("warning", f"work waiting: {harness.get('waiting_count')} claimable"
                                                 f"{from_failing_read}, slots idle")
        elif harness.get("waiting_count"):
            waiting_pill = self._pill("info", f"{harness.get('waiting_count')} claimable{from_failing_read}")
        models: list[str] = [StateParsers.model_label(model) for model in harness.get("tick_models", [])]
        cooling: str = "".join(
            self._pill("warning", f"{self._e(model['label'])} cooling {self._duration(model['remaining_s'])}")
            for model in harness.get("cooling_models", [])
        )
        return (
            "<header>"
            "<h1>Harness status</h1>"
            f'<span class="sub">{self._e(snapshot.get("repo") or "no repo")} &middot; '
            f'{self._e(snapshot.get("harness_home", ""))}</span>'
            f'<span class="flag">{state_pill}{waiting_pill}<span class="sub num">{width_text}</span></span>'
            f"{self._usage_block(usage)}"
            f'<span class="flag sub">models: {self._e(" > ".join(models) or "default")}{cooling}</span>'
            f'<span class="sub num" style="margin-left:auto">updated {self._clock(snapshot.get("generated_at"), seconds=True)}'
            "</span></header>"
        )

    def _usage_block(self, usage: JsonDict) -> str:
        """
        Renders the two usage windows as meters marked where the allocator cuts the pool.

        Args:
            usage: The snapshot's usage view.

        Returns:
            The usage HTML.
        """
        if not usage.get("available"):
            return f'<span class="pill warning"><i></i>usage unknown: {self._e(usage.get("error", ""))}</span>'
        thresholds: JsonDict = usage.get("thresholds", {})
        severity_by_band: dict[str | None, str] = {"full": "", "half": "warning", "one": "serious", "none": "critical",
                                                   None: "none"}
        rows: list[str] = []
        for label, percent, band, resets, cuts in (
            ("5-hour", usage.get("five_hour_pct"), usage.get("five_hour_band"), usage.get("five_hour_resets_at"),
             (thresholds.get("five_half_at"), thresholds.get("five_one_at"), thresholds.get("five_none_at"))),
            ("weekly", usage.get("weekly_pct"), usage.get("weekly_band"), usage.get("weekly_resets_at"),
             (thresholds.get("week_half_at"), thresholds.get("week_one_at"), thresholds.get("week_none_at"))),
        ):
            ticks: str = "".join(f'<span class="tick" style="left:{float(cut):.0f}%"></span>' for cut in
                                 (*cuts, thresholds.get("ceiling_pct")) if cut is not None)
            fill: str = f'<b class="{severity_by_band.get(band, "")}" style="width:{min(100.0, percent or 0):.0f}%"></b>'
            rows.append(
                f'<span class="sub">{label}</span><span class="meter" title="cuts the pool at '
                f'{", ".join(str(cut) for cut in cuts)} %; ceiling {thresholds.get("ceiling_pct")} %">{fill}{ticks}</span>'
                f'<span class="num">{self._percent(percent)} &middot; {self._e(band or "?")} width'
                f'{" &middot; resets " + self._clock(resets) if resets else ""}</span>'
            )
        stale: str = self._pill("warning", f"stale {self._duration(usage.get('age_s'))}") if usage.get("stale") \
            else f'<span class="sub">read {self._duration(usage.get("age_s"))} ago</span>'
        return f'<span class="usage">{"".join(rows)}</span>{stale}'

    def _machine_card(self, machine: JsonDict, snapshot: JsonDict) -> str:
        """
        Renders one machine: status, meters, sparklines, slots, card processes and containers.

        Args:
            machine: The machine view.
            snapshot: The snapshot, for the time window.

        Returns:
            The card HTML.
        """
        pills: list[str] = [self._pill(self.STATUS_LEVELS.get(machine.get("status", ""), "idle"),
                                       machine.get("status", "unknown"))]
        for signal in machine.get("signals", []):
            pills.append(self._pill(signal["level"], signal["text"]))
        gpu: JsonDict = machine.get("gpu") or {}
        facts: list[str] = []
        if gpu.get("name"):
            facts.append(self._e(gpu["name"]))
        if machine.get("preferred_rank"):
            facts.append(f"harness preference #{machine['preferred_rank']}")
        if machine.get("card_note"):
            facts.append(self._e(machine["card_note"]))
        if machine.get("slot_count"):
            facts.append(f"{machine['slot_count']} slots")
        load1: float | None = machine.get("load1")
        nproc: int | None = machine.get("nproc")
        mem_used: float | None = machine.get("mem_used_mb")
        mem_total: float | None = machine.get("mem_total_mb")
        meters: str = "".join((
            self._meter_row("CPU", load1, nproc, f"{self._number(load1)} / {nproc or '?'} cores", (0.85, 1.0, 1.5)),
            self._meter_row("Mem", mem_used, mem_total, f"{self._gb(mem_used)} / {self._gb(mem_total)} GB",
                            (0.85, 0.92, 0.97)),
            self._meter_row("GPU", gpu.get("util_pct"), 100.0, self._percent(gpu.get("util_pct")), (2.0, 2.0, 2.0)),
            self._meter_row("VRAM", gpu.get("mem_used_mb"), gpu.get("mem_total_mb"),
                            f"{self._gb(gpu.get('mem_used_mb'))} / {self._gb(gpu.get('mem_total_mb'))} GB",
                            (0.85, 0.93, 0.98)),
        ))
        link: str = ""
        if machine.get("kind") == "remote":
            upload: str = "timeout" if machine.get("upload_timed_out") else f"{self._number(machine.get('upload_mb_s'))} MB/s"
            link = (f'<div class="kv"><span>upload <b>{upload}</b></span>'
                    f'<span>connect <b>{self._number(machine.get("connect_s"), 2)} s</b></span>'
                    f'<span>probed {self._duration(machine.get("probe_age_s"))} ago</span></div>')
        history: JsonDict = machine.get("history", {})
        now: float = float(snapshot.get("generated_epoch") or 0)
        hours: float = self._window_hours(snapshot)
        sparks: str = (
            self._spark_row("GPU util", f"{hours:g} h", history.get("gpu_util", []), now, hours, 100.0, None, "%",
                            history.get("down", []))
            + self._spark_row("Load / core", f"{hours:g} h", history.get("load_per_core", []), now, hours, None, 1.0,
                              "", history.get("down", []))
        )
        slot_chips: str = "".join(
            f'<span class="chip strong">slot {slot.get("slot")} {self._issue_ref(slot.get("issue"), slot.get("holder"))}'
            f'{" " + self._e(" ".join(slot.get("tags", []))) if slot.get("tags") else ""}'
            f'{" ORPHAN" if slot.get("state") == "orphan" else ""}</span>'
            for slot in machine.get("slots", [])
        ) or '<span class="note">no slot held</span>'
        processes: list[str] = [
            f'{self._e(process.get("container") or process.get("name", ""))} pid {process.get("pid")} '
            f'{self._gb(process.get("used_mb"))} GB'
            for process in machine.get("gpu_processes", [])
        ]
        containers: list[str] = [
            f'<li><span class="line" title="{self._e(container.get("image", ""))}">{self._e(container.get("name", ""))}'
            f'</span><span class="sub">{self._e(container.get("status", ""))}</span></li>'
            for container in machine.get("containers", [])
        ]
        details: str = ""
        if processes:
            details += f'<li title="compute processes on the card">card: {"; ".join(processes)}</li>'
        if containers:
            details += (f'<li class="fold"><details><summary>containers <span class="sub">{len(containers)}</span>'
                        f'</summary><ul class="containers">{"".join(containers)}</ul></details></li>')
        error: str = f'<div class="err">live read: {self._e(machine["probe_error"])}</div>' \
            if machine.get("probe_error") else ""
        sampled: str = f'<div class="note">sampled {self._duration(machine.get("age_s"))} ago</div>' \
            if machine.get("age_s") is not None else '<div class="note">no live sample yet</div>'
        return (
            '<article class="panel">'
            f'<div class="card-head"><b title="{self._e(machine.get("name", ""))}">{self._e(machine.get("name", ""))}'
            f'</b><span class="pills">{"".join(pills)}</span></div>'
            f'<div class="note">{" &middot; ".join(facts) or "&nbsp;"}</div>'
            f'<div class="meters">{meters}</div>{link}{sparks}'
            f'<div style="margin-top:6px">{slot_chips}</div>'
            f'<ul class="list">{details}</ul>{error}{sampled}</article>'
        )

    def _meter_row(self, label: str, value: float | None, total: float | None, text: str,
                   severity_at: tuple[float, float, float]) -> str:
        """
        Renders one labelled meter whose fill colour carries severity.

        Args:
            label: The row name.
            value: The amount used.
            total: The whole.
            text: The figure printed beside the bar.
            severity_at: Fractions of the whole where the fill turns warning, serious and critical.

        Returns:
            Three grid cells.
        """
        fraction: float | None = (value / total) if value is not None and total else None
        severity: str = ""
        if fraction is not None:
            if fraction >= severity_at[2]:
                severity = "critical"
            elif fraction >= severity_at[1]:
                severity = "serious"
            elif fraction >= severity_at[0]:
                severity = "warning"
        width: float = min(100.0, max(0.0, (fraction or 0.0) * 100.0))
        bar: str = f'<b class="{severity}" style="width:{width:.1f}%"></b>' if fraction is not None else \
            '<b class="none"></b>'
        return f'<span class="k">{label}</span><span class="meter">{bar}</span><em>{text}</em>'

    def _spark_row(self, label: str, window_text: str, points: list[list[float]], now: float, hours: float,
                   fixed_max: float | None, reference: float | None, unit: str, down_epochs: list[float]) -> str:
        """
        Renders one labelled sparkline with its current value and peak.

        Args:
            label: The series name.
            window_text: The window, for the label.
            points: `[epoch, value]` pairs, oldest first.
            now: The window's right edge.
            hours: The window's length.
            fixed_max: The y maximum; None scales to the data (at least the reference line).
            reference: A value to mark with a dashed line (1.0 load a core).
            unit: Unit text after the figures.
            down_epochs: Probe times the host was down, marked under the line.

        Returns:
            The sparkline row HTML.
        """
        values: list[float] = [point[1] for point in points]
        current: str = f"{self._number(values[-1], 2 if unit == '' else 0)}{unit}" if values else "-"
        peak: str = f"peak {self._number(max(values), 2 if unit == '' else 0)}{unit}" if values else "no data"
        svg: str = self._sparkline(points, now, hours, fixed_max, reference, unit, down_epochs)
        return (f'<div class="spark"><span class="k">{label}<br>{window_text}</span>{svg}'
                f'<span class="v"><b style="color:var(--ink)">{current}</b><br>{peak}</span></div>')

    def _sparkline(self, points: list[list[float]], now: float, hours: float, fixed_max: float | None,
                   reference: float | None, unit: str, down_epochs: list[float]) -> str:
        """
        Draws a time-scaled sparkline: area, line, optional reference line, down marks and hover targets.

        A gap longer than three times the typical sample spacing breaks the line, so a missing stretch of
        probes reads as missing rather than as a straight ramp.

        Args:
            points: `[epoch, value]` pairs, oldest first.
            now: The right edge.
            hours: The window's length.
            fixed_max: The y maximum, or None to scale to the data.
            reference: Value marked with a dashed line, or None.
            unit: Unit for the hover text.
            down_epochs: Times to mark as down.

        Returns:
            The SVG element.
        """
        width: int = self.SPARK_WIDTH
        height: int = self.SPARK_HEIGHT
        start: float = now - hours * 3600
        span_s: float = max(1.0, now - start)
        visible: list[list[float]] = [point for point in points if point[0] >= start and point[1] is not None]
        y_max: float = fixed_max if fixed_max is not None else max([reference or 0.0] + [p[1] for p in visible]) * 1.1
        y_max = y_max or 1.0

        def x_of(epoch: float) -> float:
            return (epoch - start) / span_s * width

        def y_of(value: float) -> float:
            return height - 1 - min(1.0, max(0.0, value / y_max)) * (height - 3)

        spacings: list[float] = sorted(b[0] - a[0] for a, b in zip(visible, visible[1:]))
        gap_s: float = (spacings[len(spacings) // 2] * 3) if spacings else span_s
        segments: list[list[list[float]]] = []
        for point in visible:
            if not segments or point[0] - segments[-1][-1][0] > gap_s:
                segments.append([])
            segments[-1].append(point)
        parts: list[str] = []
        if reference is not None and reference <= y_max:
            parts.append(f'<line class="ref" x1="0" x2="{width}" y1="{y_of(reference):.1f}" y2="{y_of(reference):.1f}"/>')
        for segment in segments:
            coordinates: str = " ".join(f"{x_of(p[0]):.1f},{y_of(p[1]):.1f}" for p in segment)
            if len(segment) == 1:
                x: float = x_of(segment[0][0])
                parts.append(f'<rect class="area" x="{x - 1:.1f}" y="{y_of(segment[0][1]):.1f}" width="2" '
                             f'height="{height - y_of(segment[0][1]):.1f}"/>')
                continue
            parts.append(f'<polygon class="area" points="{x_of(segment[0][0]):.1f},{height} {coordinates} '
                         f'{x_of(segment[-1][0]):.1f},{height}"/>')
            parts.append(f'<polyline class="line" points="{coordinates}"/>')
        for epoch in down_epochs:
            if epoch >= start:
                parts.append(f'<rect class="down" x="{x_of(epoch) - 1:.1f}" y="{height - 3}" width="2" height="3">'
                             f'<title>{self._clock_epoch(epoch)} down</title></rect>')
        bucket_s: float = span_s / self.SPARK_HIT_BUCKETS
        for bucket in range(self.SPARK_HIT_BUCKETS):
            bucket_start: float = start + bucket * bucket_s
            in_bucket: list[float] = [p[1] for p in visible if bucket_start <= p[0] < bucket_start + bucket_s]
            if not in_bucket:
                continue
            decimals: int = 2 if unit == "" else 0
            text: str = (f"{self._clock_epoch(bucket_start)}: max {self._number(max(in_bucket), decimals)}{unit}, "
                         f"mean {self._number(sum(in_bucket) / len(in_bucket), decimals)}{unit}")
            parts.append(f'<rect class="hit" x="{x_of(bucket_start):.1f}" y="0" width="{width / self.SPARK_HIT_BUCKETS:.1f}" '
                         f'height="{height}"><title>{text}</title></rect>')
        if not visible:
            parts.append(f'<text x="{width / 2}" y="{height / 2 + 4}" text-anchor="middle" font-size="10" '
                         'fill="currentColor" opacity=".5">no samples in window</text>')
        return (f'<svg viewBox="0 0 {width} {height}" preserveAspectRatio="none" role="img" '
                f'aria-label="{len(visible)} samples">{"".join(parts)}</svg>')

    def _ticks_panel(self, snapshot: JsonDict) -> str:
        """
        Renders the live ticks.

        Args:
            snapshot: The snapshot.

        Returns:
            The panel HTML.
        """
        ticks: list[JsonDict] = snapshot.get("ticks", [])
        rows: list[str] = []
        for tick in ticks:
            if tick.get("issue") is not None:
                issue_cell: str = (f'{self._issue_link(tick["issue"], tick.get("url"))} '
                                   f'<span title="{self._e(tick.get("title", ""))}">{self._e(tick.get("title", ""))}</span>')
            elif tick.get("choosing"):
                issue_cell = '<span class="note">choosing an issue</span>'
            else:
                issue_cell = '<span class="err">issue unknown</span>'
            lane_level: str = {"fable": "serious", "landing": "info"}.get(tick.get("lane", ""), "good")
            model_text: str = tick.get("model_label", "?") + (" landing" if tick.get("lane") == "landing" else "")
            doing: str = tick.get("doing") or ""
            doing_cell: str = (f'<span title="{self._e(doing)}">{self._duration(tick.get("doing_age_s"))} '
                               f'{self._e(doing)}</span>') if doing \
                else '<span class="note">thinking</span>'
            leases: str = " ".join(f"#{slot}" for slot in tick.get("lease_slots", [])) or "-"
            rows.append(
                f"<tr><td>{issue_cell}</td><td>{self._pill(lane_level, model_text)}</td>"
                f"<td>{tick.get('slot') or '-'}</td><td>{self._duration(tick.get('age_s'))}</td>"
                f"<td>{leases}</td><td>{doing_cell}</td>"
                f'<td class="sub">{tick.get("pid")} <span class="chip">{self._e(tick.get("issue_source") or "-")}</span></td></tr>'
            )
        table: str = (
            '<table><colgroup><col style="width:34%"><col style="width:96px"><col style="width:40px">'
            '<col style="width:56px"><col style="width:56px"><col><col style="width:118px"></colgroup>'
            "<tr><th>Issue</th><th>Model</th><th>Slot</th><th>Age</th><th>Leases</th><th>Now</th><th>PID / from</th></tr>"
            f'{"".join(rows)}</table>'
        ) if rows else '<div class="empty">no tick running</div>'
        return (f'<article class="panel"><h2>Running ticks <span class="sub">{len(ticks)} live</span></h2>'
                f'<div class="hscroll">{table}</div></article>')

    def _leases_panel(self, snapshot: JsonDict) -> str:
        """
        Renders the GPU slots, whole-card holds, device lease, operator priority and waits.

        Args:
            snapshot: The snapshot.

        Returns:
            The panel HTML.
        """
        leases: JsonDict = snapshot.get("leases", {})
        if not leases.get("available"):
            return ('<article class="panel"><h2>GPU slots and leases</h2>'
                    f'<div class="err">{self._e(leases.get("error") or "lease.sh status not read")}</div></article>')
        rows: list[str] = []
        for slot in leases.get("slots", []):
            state: str = slot.get("state", "")
            holder_kind: str = slot.get("holder_kind") or ""
            rows.append(
                f'<tr class="{"dim" if state == "free" else ""}"><td>{slot.get("slot")}</td>'
                f'<td title="{self._e(slot.get("host", ""))}">{self._e(self._short_host(slot.get("host", "")))}</td>'
                f"<td>{self._pill(self.STATUS_LEVELS.get(state, 'idle'), state)}</td>"
                f"<td>{self._issue_ref(slot.get('issue'), slot.get('holder')) if state != 'free' else ''}</td>"
                f'<td title="{self._e(holder_kind + " " + " ".join(slot.get("tags", [])))}">'
                f'{self._e(holder_kind.split(" ", 1)[0])} '
                f'{"".join(f"<span class=chip>{self._e(tag)}</span>" for tag in slot.get("tags", []))}</td>'
                f'<td>{self._duration(slot.get("held_s")) if state != "free" else ""}</td></tr>'
            )
        table: str = (
            '<table><colgroup><col style="width:36px"><col style="width:27%"><col style="width:74px">'
            '<col style="width:58px"><col><col style="width:52px"></colgroup>'
            "<tr><th>Slot</th><th>Host</th><th>State</th><th>Issue</th><th>Holder and tags</th><th>Held</th></tr>"
            f'{"".join(rows)}</table>'
        )
        notes: list[str] = []
        for hold in leases.get("whole_card_holds", []):
            verb: str = "reserving" if hold.get("reserving") else "holds"
            notes.append(f'<li>{self._pill("info", "whole card")} {self._issue_ref(hold.get("issue"), hold.get("holder"))} '
                         f'{verb} {self._e(self._short_host(hold.get("host", "")))} '
                         f'(slots {", ".join(str(slot) for slot in hold.get("slots", []))}) for '
                         f'{self._duration(hold.get("held_s"))}, holder {self._e(hold.get("holder_kind", ""))}</li>')
        device: JsonDict | None = leases.get("device")
        if device:
            notes.append(f'<li>{self._pill(self.STATUS_LEVELS.get(device.get("state", ""), "idle"), "device " + device.get("state", ""))} '
                         f'{self._issue_ref(device.get("issue"), device.get("holder"))} for {self._duration(device.get("held_s"))}</li>')
        priority: list[str] = leases.get("operator_priority", [])
        notes.append('<li>operator priority: ' + (" > ".join(self._issue_ref(int(item) if item.isdigit() else None, item)
                                                            for item in priority) or '<span class="note">none</span>')
                     + "</li>")
        for wait in leases.get("waits", []):
            notes.append(f'<li>{self._pill("warning", "waiting")} {self._issue_ref(wait.get("issue"), None)} for '
                         f'{self._e(wait.get("kind", ""))} {wait.get("minutes")} min'
                         f'{" (" + self._e(wait["tag"]) + ")" if wait.get("tag") else ""}</li>')
        if leases.get("harness_only_hosts"):
            notes.append(f'<li>harness hosts, preferred first: {self._e(" > ".join(leases["harness_only_hosts"]))}</li>')
        if leases.get("local_card_off"):
            notes.append("<li>local card off: every run goes to a remote card</li>")
        for line in leases.get("unparsed", []):
            notes.append(f'<li class="note">{self._e(line)}</li>')
        error: str = f'<div class="err">last read failed: {self._e(leases["error"])}</div>' if leases.get("error") else ""
        held: int = sum(1 for slot in leases.get("slots", []) if slot.get("state") != "free")
        return (f'<article class="panel"><h2>GPU slots and leases <span class="sub">{held} of '
                f'{len(leases.get("slots", []))} held</span></h2><div class="hscroll">{table}</div>'
                f'<ul class="list">{"".join(notes)}</ul>{error}</article>')

    def _queue_section(self, snapshot: JsonDict) -> str:
        """
        Renders one panel per issue group, sign-off stacked under claimable since both are usually short. The
        claimable panel leads with the dashboard's own count, sets claimable.sh's and the allocator's beside it,
        and lists under it what the first-before-low gate holds back.

        Args:
            snapshot: The snapshot.

        Returns:
            The queue section HTML.
        """
        queue: JsonDict = snapshot.get("queue", {})
        groups: JsonDict = queue.get("groups", {})
        labels: JsonDict = snapshot.get("harness", {}).get("labels", {})
        now: float = float(snapshot.get("generated_epoch") or 0)
        panels: dict[str, str] = {}
        for name, title in self.QUEUE_TITLES.items():
            issues: list[JsonDict] = groups.get(name, [])
            extra: str = ""
            rows: str = "".join(self._issue_row(issue, now, labels) for issue in issues) or \
                '<tr><td class="empty" colspan="2">none</td></tr>'
            if name == "claimable":
                extra = self._claimable_counts(queue, snapshot.get("harness", {}).get("queue_label", ""))
                held: list[JsonDict] = groups.get("held", [])
                if held:
                    rows += (f'<tr><td colspan="2" class="note"><b>held back by the first-before-low gate: '
                             f'{len(held)}</b></td></tr>')
                    rows += "".join(self._issue_row(issue, now, labels) for issue in held)
            panels[name] = (
                f'<article class="panel"><h2>{title} <span class="sub">{len(issues)}{extra}</span></h2>'
                '<div class="scroll"><table><colgroup><col><col style="width:60px"></colgroup>'
                f"{rows}</table></div></article>"
            )
        status: str = ""
        if queue.get("error"):
            status = f'<div class="err">GitHub: {self._e(queue["error"])}</div>'
        age: str = f'issues read {self._duration(queue.get("age_s"))} ago' if queue.get("age_s") is not None \
            else "issues not read yet"
        return (f'<section><div class="note" style="margin:0 2px 6px">Issue queue &middot; {age}</div>{status}'
                f'<div class="grid queue">{panels["running"]}<div class="stack">{panels["claimable"]}'
                f'{panels["signoff"]}</div>{panels["operator"]}{panels["other"]}</div></section>')

    def _claimable_counts(self, queue: JsonDict, queue_label: str) -> str:
        """
        Words claimable.sh's and the allocator's counts beside the dashboard's own, each marked unverified when the
        dashboard's own GitHub read was failing as it was taken: claimable.sh prints 1 when its own read fails.

        Args:
            queue: The snapshot's queue view.
            queue_label: The label claimable.sh counts.

        Returns:
            The header text after the count.
        """
        unverified: str = self._pill("warning", "unverified")
        text: str = f" from this read ({self._e(queue_label)})"
        claimable_sh: JsonDict = queue.get("claimable_sh") or {}
        if claimable_sh.get("value") is not None:
            text += (f" &middot; claimable.sh {claimable_sh['value']}, {self._duration(claimable_sh.get('age_s'))} ago"
                     f"{' ' + unverified if claimable_sh.get('unverified') else ''}")
        elif claimable_sh.get("error"):
            text += f' &middot; <span class="err">claimable.sh: {self._e(claimable_sh["error"])}</span>'
        allocator: JsonDict | None = queue.get("allocator_claimable")
        if allocator:
            text += (f" &middot; allocator {self._e(allocator.get('value'))} at {self._clock(allocator.get('at'))}"
                     f"{' ' + unverified if allocator.get('unverified') else ''}")
        return text

    def _issue_row(self, issue: JsonDict, now: float, labels: JsonDict) -> str:
        """
        Renders one issue: number and title, then its labels other than the queue's own and why it is in its
        group, then the age of its last comment.

        Args:
            issue: The issue.
            now: The current epoch.
            labels: The harness's label names (QueueLabels fields).

        Returns:
            The table row.
        """
        all_label: str = labels.get("all_issues", "ai")
        structural: frozenset[str] = frozenset(labels.get(key, "") for key in
                                               ("all_issues", "queue", "wip", "operator", "signoff"))
        strong: frozenset[str] = frozenset(labels.get(key, "") for key in ("first", "fable"))
        flags: str = "".join(f'<span class="chip{" strong" if label in strong else ""}">'
                             f'{self._e(label.removeprefix(all_label + ":"))}</span>'
                             for label in issue.get("labels", []) if label not in structural)
        last_comment: str | None = issue.get("last_comment_at") or issue.get("updated_at")
        age: str = self._duration(now - self._epoch(last_comment)) if last_comment else "-"
        title: str = self._e(issue.get("title", ""))
        return (
            f'<tr><td title="{title}">{self._issue_link(issue.get("number"), issue.get("url"))} {title}'
            f'<div class="note line">{flags}{self._e(issue.get("reason", ""))}</div></td>'
            f'<td class="sub" title="last comment {self._e(last_comment or "")}">{age}</td></tr>'
        )

    def _landings_panel(self, snapshot: JsonDict) -> str:
        """
        Renders the PRs merged in the last 24 hours.

        Args:
            snapshot: The snapshot.

        Returns:
            The panel HTML.
        """
        now: float = float(snapshot.get("generated_epoch") or 0)
        landings: list[JsonDict] = snapshot.get("landings", [])
        rows: str = "".join(
            f'<tr><td title="{self._e(pull.get("title", ""))}">{self._issue_link(pull.get("number"), pull.get("url"))} '
            f'{self._e(pull.get("title", ""))}</td><td><span class="chip">{self._e(pull.get("baseRefName", ""))}</span></td>'
            f'<td class="sub">{self._duration(now - self._epoch(pull.get("mergedAt")))}</td></tr>'
            for pull in landings
        ) or '<tr><td class="empty">none in the last 24 h</td></tr>'
        return (f'<article class="panel"><h2>Landings <span class="sub">{len(landings)} merged in 24 h</span></h2>'
                '<div class="scroll"><table><colgroup><col><col style="width:110px"><col style="width:60px"></colgroup>'
                f"{rows}</table></div></article>")

    def _stalls_panel(self, snapshot: JsonDict) -> str:
        """
        Renders the stall watch's newest lines.

        Args:
            snapshot: The snapshot.

        Returns:
            The panel HTML.
        """
        now: float = float(snapshot.get("generated_epoch") or 0)
        stalls: list[JsonDict] = list(reversed(snapshot.get("health", {}).get("stalls", [])))
        level_by_kind: dict[str, str] = {"lock_wait": "warning", "gpu_idle": "warning", "host_degraded": "serious"}
        rows: str = "".join(
            f'<tr><td class="sub">{self._duration(now - self._epoch(stall.get("time")))}</td>'
            f'<td>{self._pill(level_by_kind.get(stall.get("kind", ""), "info"), stall.get("kind", ""))}</td>'
            f'<td title="{self._e(stall.get("detail", ""))}"><div class="clamp"><b>{self._e(self._short_host(stall.get("key", "")))}</b> '
            f'{self._e(self._clip(stall.get("detail", ""), 220))}</div></td></tr>'
            for stall in stalls
        ) or '<tr><td class="empty">no stall logged</td></tr>'
        return (f'<article class="panel"><h2>Stalls <span class="sub">state/stalls.log, newest first</span></h2>'
                '<div class="scroll"><table><colgroup><col style="width:60px"><col style="width:120px"><col></colgroup>'
                f"{rows}</table></div></article>")

    def _allocator_panel(self, snapshot: JsonDict) -> str:
        """
        Renders the allocator's last decisions.

        Args:
            snapshot: The snapshot.

        Returns:
            The panel HTML.
        """
        now: float = float(snapshot.get("generated_epoch") or 0)
        lines: list[JsonDict] = list(reversed(snapshot.get("health", {}).get("allocator", [])))
        rows: list[str] = []
        for line in lines:
            fields: JsonDict = line.get("fields", {})
            summary: str = " ".join(f"{key}={self._e(fields[key])}" for key in
                                    ("five", "week", "pct", "allowed", "busy", "claimable", "young", "ready",
                                     "spawning") if key in fields)
            if line.get("claimable_unverified"):
                summary += " " + self._pill("warning", "claimable unverified")
            rows.append(
                f'<tr><td class="sub">{self._duration(now - self._epoch(line.get("time")))}</td>'
                f'<td><span class="chip strong">{self._e(line.get("kind", ""))}</span>'
                f'{self._e(line.get("slot") or "")}</td>'
                f'<td title="{self._e(line.get("text", ""))}">{summary or self._e(line.get("text", ""))}</td></tr>'
            )
        body: str = "".join(rows) or '<tr><td class="empty">no allocator line</td></tr>'
        return (f'<article class="panel"><h2>Allocator <span class="sub">state/logs/allocator.log, newest first</span>'
                '</h2><div class="scroll"><table><colgroup><col style="width:60px"><col style="width:110px"><col>'
                f"</colgroup>{body}</table></div></article>")

    def _flow_section(self, snapshot: JsonDict) -> str:
        """
        Renders the flow diagram: devices on top, issue cards in state columns under them, a line from each card
        to each device lane it holds (solid) or waits for (dashed), and the newest transitions under it. A card
        that moved since the last diagram starts where it was and slides to its place; new cards and lines fade
        in. Positions come from the snapshot, so the page needs no script.

        Args:
            snapshot: The snapshot.

        Returns:
            The section HTML, or an empty string when the snapshot has no diagram.
        """
        flow: JsonDict | None = snapshot.get("flow")
        if not flow:
            return ""
        width: int = int(flow["width"])
        height: int = int(flow["height"])
        moved_issues: frozenset[int] = frozenset(card["issue"] for card in flow["cards"] if card.get("moved"))
        paths: list[str] = []
        for edge in flow["edges"]:
            x1, y1, x2, y2 = (float(edge[key]) for key in ("x1", "y1", "x2", "y2"))
            bend: float = max(40.0, (y1 - y2) / 2)
            state: str = "late" if edge["issue"] in moved_issues else ("fresh" if edge.get("fresh") else "")
            paths.append(
                f'<path class="fedge {self._e(edge["lane"])} {self._e(edge["kind"])} {state}" '
                f'd="M{x1:.0f},{y1:.0f} C{x1:.0f},{y1 - bend:.0f} {x2:.0f},{y2 + bend:.0f} {x2:.0f},{y2:.0f}">'
                f'<title>#{self._e(edge["issue"])} {self._e(edge["kind"])} the {self._e(edge["lane"])} lane of '
                f'{self._e(edge["device"])}</title></path>'
            )
        parts: list[str] = [f'<svg class="flow-edges" width="{width}" height="{height}" viewBox="0 0 {width} {height}" '
                            f'aria-hidden="true">{"".join(paths)}</svg>']
        for device in flow["devices"]:
            parts.append(self._flow_device(device))
        for column in flow["columns"]:
            parts.append(f'<div class="fcol" style="left:{column["x"]}px;top:{column["y"]}px;width:{column["w"]}px">'
                         f'{self._e(column["title"])} <span class="sub">{column["count"]}</span></div>')
            if column["hidden"]:
                parts.append(f'<div class="fmore" style="left:{column["x"] + 4}px;top:{column["more_y"]}px">'
                             f'+{column["hidden"]} more</div>')
        for card in flow["cards"]:
            parts.append(self._flow_card(card))
        legend: str = (
            '<div class="flow-legend">'
            '<span><svg width="28" height="8"><line x1="0" y1="4" x2="28" y2="4" class="fedge card holds"/></svg> '
            'holds a card</span>'
            '<span><svg width="28" height="8"><line x1="0" y1="4" x2="28" y2="4" class="fedge card waits"/></svg> '
            'waits for a card</span>'
            '<span><svg width="28" height="8"><line x1="0" y1="4" x2="28" y2="4" class="fedge compile holds"/></svg>'
            ' holds a compile lane</span>'
            '<span><svg width="28" height="8"><line x1="0" y1="4" x2="28" y2="4" class="fedge compile waits"/></svg>'
            ' waits for a compile lane</span>'
            '<span>cards slide to their new state on each refresh</span></div>'
        )
        trail: str = "".join(
            f'<li title="{self._e(transition["from"])} → {self._e(transition["to"])}">'
            f'<span class="sub">{self._clock(transition["time"])}</span> <b>#{self._e(transition["issue"])}</b> '
            f'{self._e(transition["from"])} → {self._e(transition["to"])}</li>'
            for transition in flow.get("transitions", [])[:8]
        ) or '<li class="note">no transition seen yet</li>'
        return (
            '<section class="panel"><h2>Flow <span class="sub">issues by state, and the device each holds or waits '
            'for</span></h2>'
            f'{legend}<div class="flow-scroll"><div class="flow-canvas" style="width:{width}px;height:{height}px">'
            f'{"".join(parts)}</div></div><ol class="flow-trail">{trail}</ol></section>'
        )

    def _flow_device(self, device: JsonDict) -> str:
        """
        Renders one device node: card use, load, card memory, lease holders, compile lane and the idle flag.

        Args:
            device: A device from the snapshot's flow.

        Returns:
            The node HTML.
        """
        util: float | None = device.get("gpu_util_pct")
        flag: str = self._pill("warning", device.get("idle_flag") or "card idle, queue waiting") \
            if device.get("idle_with_queue") else ""
        status: str = self._pill(self.STATUS_LEVELS.get(device.get("status") or "", "idle"),
                                 device.get("status") or "unknown")
        holders: str = ", ".join(self._e(holder) for holder in device.get("holders", [])) or "no lease held"
        memory: str = f'{self._gb(device.get("gpu_mem_used_mb"))} / {self._gb(device.get("gpu_mem_total_mb"))} GB'
        load: str = f'{self._number(device.get("load1"))} / {device.get("nproc") or "?"} cores'
        width: float = min(100.0, max(0.0, float(util or 0)))
        card_anchor: float = device["w"] * 0.3
        compile_anchor: float = device["w"] * 0.75
        return (
            f'<div class="fdev{" flagged" if device.get("idle_with_queue") else ""}" '
            f'style="left:{device["x"]}px;top:{device["y"]}px;width:{device["w"]}px;height:{device["h"]}px">'
            f'<div class="top"><span><b>{self._e(device["tag"])}</b> <span class="sub">{self._e(device["name"])}'
            f'</span></span><span class="pills">{flag}{status}</span></div>'
            f'<div class="sub">{self._e(device.get("gpu_name") or "card unknown")}</div>'
            f'<div style="display:grid;grid-template-columns:auto 1fr auto;gap:6px;align-items:center">'
            f'<span class="sub">GPU</span><span class="meter"><b style="width:{width:.0f}%"></b></span>'
            f'<span class="num">{self._percent(util)}</span></div>'
            f'<div class="sub num">load {load} &middot; card memory {memory}</div>'
            f'<div class="holders" title="{holders}">lease: {holders}</div>'
            f'<div class="sub">{self._e(device.get("compile", ""))}</div>'
            f'<div class="anchors"><span style="left:{card_anchor:.0f}px">▾ card</span>'
            f'<span style="left:{compile_anchor:.0f}px">▾ compile</span></div></div>'
        )

    def _flow_card(self, card: JsonDict) -> str:
        """
        Renders one issue card at its place; a moved card starts at its old place, a new one fades in.

        Args:
            card: A card from the snapshot's flow.

        Returns:
            The card HTML.
        """
        motion: str = ""
        classes: str = f'fcard {self._e(card["column"])}'
        if card.get("moved") and card.get("from_x") is not None:
            classes += " moved"
            motion = f';--dx:{float(card["from_x"]) - float(card["x"]):.0f}px;--dy:{float(card["from_y"]) - float(card["y"]):.0f}px'
        elif card.get("fresh"):
            classes += " fresh"
        chips: str = "".join(f'<span class="chip">{self._e(chip)}</span>' for chip in card.get("chips", []))
        hover: str = f'#{card["issue"]} {card.get("title", "")}: {card.get("detail", "")}'
        return (
            f'<div class="{classes}" style="left:{card["x"]}px;top:{card["y"]}px;width:{card["w"]}px;'
            f'height:{card["h"]}px{motion}" title="{self._e(hover)}">'
            f'<div class="l1">{self._issue_link(card["issue"], card.get("url"))}{chips}</div>'
            f'<div class="l2">{self._e(card.get("detail", ""))}</div>'
            f'<div class="l3">{self._e(card.get("title", ""))}</div></div>'
        )

    def _sources_footer(self, snapshot: JsonDict, token_query: str) -> str:
        """
        Renders every source's age, cadence, duration and last error.

        Args:
            snapshot: The snapshot.
            token_query: The access token query, for the JSON link.

        Returns:
            The footer HTML.
        """
        rows: str = "".join(
            f"<tr><td>{self._e(name)}</td><td>{self._duration(source.get('age_s')) if source.get('age_s') is not None else 'never'}</td>"
            f"<td>every {self._duration(source.get('interval_s'))}</td>"
            f"<td>{'' if source.get('duration_s') is None else str(source.get('duration_s')) + ' s'}</td>"
            f'<td class="{"err" if source.get("error") else ""}">{self._e(source.get("error") or "")}</td></tr>'
            for name, source in snapshot.get("sources", {}).items()
            if source.get("interval_s", 0) > 0 or source.get("age_s") is not None
        )
        return ('<footer class="panel"><div class="hscroll"><table><colgroup><col style="width:120px"><col style="width:70px">'
                '<col style="width:90px"><col style="width:70px"><col></colgroup>'
                f"<tr><th>Source</th><th>Age</th><th>Cadence</th><th>Took</th><th>Last error</th></tr>{rows}</table></div>"
                f'<div style="margin-top:4px">JSON: <a href="/api/state{self._e(token_query)}">/api/state</a></div>'
                '</footer>')

    def _pill(self, level: str, text: Any) -> str:
        """
        Args:
            level: good, info, warning, serious, critical, idle, or a status the STATUS_LEVELS table maps.
            text: The words.

        Returns:
            A status pill: shape, colour and words.
        """
        css_level: str = self.STATUS_LEVELS.get(level, level)
        return f'<span class="pill {self._e(css_level)}"><i></i>{self._e(text)}</span>'

    def _issue_link(self, number: Any, url: Any) -> str:
        """
        Args:
            number: The issue or PR number.
            url: Its URL, if known.

        Returns:
            `#N`, linked when the URL is known.
        """
        if number is None:
            return ""
        if url:
            return f'<a href="{self._e(url)}" target="_blank" rel="noopener noreferrer">#{self._e(number)}</a>'
        return f"#{self._e(number)}"

    def _issue_ref(self, issue: Any, holder: Any) -> str:
        """
        Args:
            issue: The issue number, or None.
            holder: The lease holder name, shown when there is no issue number.

        Returns:
            `#N` or the holder name.
        """
        return f"#{self._e(issue)}" if issue is not None else self._e(holder or "")

    def _window_hours(self, snapshot: JsonDict) -> float:
        """
        Args:
            snapshot: The snapshot.

        Returns:
            The sparkline window in hours.
        """
        return float(snapshot.get("history_hours") or 6)

    @staticmethod
    def _e(value: Any) -> str:
        """
        Args:
            value: Any value.

        Returns:
            Its text, HTML-escaped for element and attribute content.
        """
        return html.escape("" if value is None else str(value), quote=True)

    @staticmethod
    def _duration(seconds: Any) -> str:
        """
        Args:
            seconds: A duration.

        Returns:
            `45s`, `12m`, `3h05m` or `2d04h`; `-` when unknown.
        """
        if seconds is None:
            return "-"
        total: int = max(0, int(float(seconds)))
        if total < 60:
            return f"{total}s"
        if total < 3600:
            return f"{total // 60}m"
        if total < 86400:
            return f"{total // 3600}h{(total % 3600) // 60:02d}m"
        return f"{total // 86400}d{(total % 86400) // 3600:02d}h"

    @staticmethod
    def _epoch(iso_text: Any) -> float:
        """
        Args:
            iso_text: An ISO-8601 UTC time.

        Returns:
            Epoch seconds, or 0 when unreadable.
        """
        if not iso_text:
            return 0.0
        try:
            return datetime.fromisoformat(str(iso_text).replace("Z", "+00:00")).timestamp()
        except ValueError:
            return 0.0

    @staticmethod
    def _clock(iso_text: Any, seconds: bool = False) -> str:
        """
        Args:
            iso_text: An ISO-8601 UTC time.
            seconds: Include seconds.

        Returns:
            `HH:MMZ` (with the date when it is not today, UTC), or `-`.
        """
        epoch: float = HtmlRenderer._epoch(iso_text)
        return HtmlRenderer._clock_epoch(epoch, seconds) if epoch else "-"

    @staticmethod
    def _clock_epoch(epoch: float, seconds: bool = False) -> str:
        """
        Args:
            epoch: Epoch seconds.
            seconds: Include seconds.

        Returns:
            `HH:MMZ`, prefixed with `MM-DD` when not today (UTC).
        """
        moment: datetime = datetime.fromtimestamp(epoch, tz=timezone.utc)
        today: bool = moment.date() == datetime.now(tz=timezone.utc).date()
        clock: str = moment.strftime("%H:%M:%S" if seconds else "%H:%M") + "Z"
        return clock if today else moment.strftime("%m-%d ") + clock

    @staticmethod
    def _number(value: Any, decimals: int = 1) -> str:
        """
        Args:
            value: A number or None.
            decimals: Decimal places.

        Returns:
            The number, or `-`.
        """
        if value is None:
            return "-"
        return f"{float(value):.{decimals}f}"

    @staticmethod
    def _percent(value: Any) -> str:
        """
        Args:
            value: A percentage or None.

        Returns:
            `63 %`, or `-`.
        """
        return "-" if value is None else f"{float(value):.0f} %"

    @staticmethod
    def _gb(megabytes: Any) -> str:
        """
        Args:
            megabytes: MiB or None.

        Returns:
            GiB to one decimal, or `-`.
        """
        return "-" if megabytes is None else f"{float(megabytes) / 1024:.1f}"

    @staticmethod
    def _short_host(host: str) -> str:
        """
        Args:
            host: `user@host` or `local`.

        Returns:
            The host without the user.
        """
        return host.split("@", 1)[-1]

    @staticmethod
    def _clip(text: str, limit: int) -> str:
        """
        Args:
            text: Any text.
            limit: Maximum characters.

        Returns:
            The text, cut with an ellipsis past the limit.
        """
        return text if len(text) <= limit else text[: limit - 1] + "…"
