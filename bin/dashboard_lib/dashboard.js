"use strict";

(() => {
    const interval = Math.max(1, Number(document.body.dataset.refresh)) * 1000;
    const token = new URLSearchParams(location.search).get("t");
    const sections = new Map(Array.from(document.querySelectorAll("[data-section]"),
        node => [node.dataset.section, node]));
    let validator = "";
    let timer;
    let polling = false;

    function url(path) {
        const address = new URL(path, location.origin);
        address.searchParams.set("t", token);
        return address;
    }

    function key(node) {
        return node.dataset?.key || node.nodeName + " " + (node.classList?.[0] || "");
    }

    function patchChildren(parent, source) {
        const siblings = new Map();
        for (const child of parent.childNodes) {
            const id = key(child);
            if (!siblings.has(id)) siblings.set(id, []);
            siblings.get(id).push(child);
        }
        // Absolute positions and SVG paths determine diagram order. Reparenting restarts CSS animations.
        const positioned = parent.matches(".flow-canvas,.flow-edges");
        let cursor = parent.firstChild;
        for (const fresh of Array.from(source.childNodes)) {
            const current = siblings.get(key(fresh))?.shift();
            if (!current) {
                if (positioned && fresh.matches?.(".fcard,.fedge") && !fresh.matches(".moved,.late")) {
                    fresh.classList.add("fresh");
                }
                parent.insertBefore(fresh, positioned ? null : cursor);
                continue;
            }
            if (!positioned && current !== cursor) parent.insertBefore(current, cursor);
            cursor = current.nextSibling;
            if (current.isEqualNode(fresh)) continue;
            if (current.nodeType === Node.TEXT_NODE) {
                current.data = fresh.data;
                continue;
            }
            if (current.matches(".fcard") &&
                    (current.style.left !== fresh.style.left || current.style.top !== fresh.style.top)) {
                const dx = parseFloat(current.style.left) - parseFloat(fresh.style.left);
                const dy = parseFloat(current.style.top) - parseFloat(fresh.style.top);
                fresh.classList.remove("fresh", "moved");
                if (!matchMedia("(prefers-reduced-motion: reduce)").matches) {
                    current.animate([{transform: `translate(${dx}px, ${dy}px)`}, {transform: "none"}],
                        {duration: 1100, easing: "cubic-bezier(.2,.7,.2,1)"});
                }
            } else if (current.matches(".fcard,.fedge") && current.style.left === fresh.style.left &&
                    current.style.top === fresh.style.top && current.getAttribute("d") === fresh.getAttribute("d")) {
                for (const name of ["fresh", "moved", "late"]) {
                    fresh.classList.toggle(name, current.classList.contains(name));
                }
                for (const name of ["--dx", "--dy"]) {
                    fresh.style.setProperty(name, current.style.getPropertyValue(name));
                }
            }
            for (const name of current.getAttributeNames()) {
                if (name === "open" && current.localName === "details") continue;
                if (!fresh.hasAttribute(name)) current.removeAttribute(name);
            }
            for (const attr of fresh.attributes) {
                if (current.getAttribute(attr.name) !== attr.value) current.setAttribute(attr.name, attr.value);
            }
            patchChildren(current, fresh);
        }
        for (const remaining of siblings.values()) {
            for (const child of remaining) child.remove();
        }
    }

    function updateAges() {
        const now = Date.now();
        for (const stamp of document.querySelectorAll("time[datetime]")) {
            const seconds = Math.max(0, Math.floor((now - Date.parse(stamp.dateTime)) / 1000));
            if (!Number.isFinite(seconds)) continue;
            const minutes = Math.floor(seconds / 60);
            const hours = Math.floor(minutes / 60);
            const pad = value => String(value).padStart(2, "0");
            const age = seconds < 60 ? seconds + "s" : minutes < 60 ? minutes + "m" :
                hours < 24 ? hours + "h" + pad(minutes % 60) + "m" : Math.floor(hours / 24) + "d" + pad(hours % 24) + "h";
            if (stamp.textContent !== age + " ago") stamp.textContent = age + " ago";
        }
    }

    async function poll() {
        clearTimeout(timer);
        if (polling || document.hidden) return;
        polling = true;
        try {
            const response = await fetch(url("/api/sections"), {cache: "no-store",
                headers: validator ? {"If-None-Match": validator} : {}});
            if (response.status === 304) return;
            if (!response.ok) throw Error("Section index: " + response.status);
            const versions = await response.json();
            const changed = Object.entries(versions).filter(([name, hash]) => sections.get(name)?.dataset.version !== hash);
            const updates = await Promise.all(changed.map(async ([name, hash]) => {
                const fragment = await fetch(url("/api/sections/" + hash));
                if (!fragment.ok) throw Error("Section " + name + ": " + fragment.status);
                const template = document.createElement("template");
                template.innerHTML = await fragment.text();
                return [name, hash, template.content];
            }));
            const machines = document.querySelector(".machines");
            for (const [name, hash, content] of updates) {
                let slot = sections.get(name);
                if (!slot) {
                    slot = document.createElement("div");
                    slot.dataset.section = name;
                    sections.set(name, slot);
                }
                patchChildren(slot, content);
                slot.dataset.version = hash;
            }
            for (const [name, slot] of sections) {
                if (!Object.hasOwn(versions, name)) {
                    slot.remove();
                    sections.delete(name);
                }
            }
            let cursor = machines.firstElementChild;
            for (const name of Object.keys(versions).filter(name => name.startsWith("machine-"))) {
                const slot = sections.get(name);
                if (slot !== cursor) machines.insertBefore(slot, cursor);
                cursor = slot.nextElementSibling;
            }
            validator = response.headers.get("ETag");
        } catch (error) {
            console.warn("Dashboard update deferred", error.message);
        } finally {
            polling = false;
            if (!document.hidden) {
                updateAges();
                timer = setTimeout(poll, interval);
            }
        }
    }

    document.addEventListener("visibilitychange", () => {
        clearTimeout(timer);
        if (!document.hidden) poll();
    });
    updateAges();
    if (!document.hidden) timer = setTimeout(poll, interval);
})();
