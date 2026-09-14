// Narrow one-way evidence collector on https://chatgpt.com/*
// Collects canonical existing conversation URL, rendered transcript text, and account evidence.
// Does NOT have access to pairing secrets, native messaging, or execution control.

(() => {
  // Stable per-document identity generated on script load
  const DOCUMENT_ID = "doc_" + (globalThis.crypto?.randomUUID ? globalThis.crypto.randomUUID().replace(/-/g, "").slice(0, 16) : Math.random().toString(36).slice(2, 18));

  function getCanonicalConversationId() {
    const pathname = window.location.pathname;
    const segments = pathname.split("/").filter(Boolean);
    let conversationId = null;
    if (segments.length === 2 && segments[0] === "c") {
      conversationId = segments[1];
    } else if (segments.length === 4 && segments[0] === "g" && segments[2] === "c") {
      conversationId = segments[3];
    }
    if (!conversationId || conversationId === "new" || conversationId === "chat" || conversationId.includes("new_chat") || conversationId.includes("provisional")) {
      return null;
    }
    return conversationId;
  }

  function getRenderedTranscriptText() {
    const turnSelectors = [
      'main [data-testid^="conversation-turn"]',
      'article',
      '[data-message-author-role]'
    ];
    let turnNodes = [];
    for (const selector of turnSelectors) {
      const candidates = Array.from(document.querySelectorAll(selector));
      if (candidates.length > 0) {
        turnNodes = candidates;
        break;
      }
    }
    let transcriptText = "";
    for (const node of turnNodes) {
      const text = (node.innerText || "").trim();
      if (text) {
        transcriptText += text + "\n";
        if (transcriptText.length > 128 * 1024) {
          transcriptText = transcriptText.slice(0, 128 * 1024);
          break;
        }
      }
    }
    return transcriptText;
  }

  function getAccountContextText() {
    const accountSelectors = [
      '[data-testid*="user-profile"], [data-testid*="workspace"], button[id*="user-menu"]',
      '[aria-label*="profile menu" i]'
    ];

    let accountText = "";
    for (const selector of accountSelectors) {
      const accountNodes = document.querySelectorAll(selector);
      for (const node of accountNodes) {
        const visibleText = (node?.innerText || "").trim();
        if (visibleText) {
          accountText = visibleText;
          break;
        }
        const rawAriaLabel = (node?.getAttribute?.("aria-label") || "").trim();
        const cleanedAriaLabel = rawAriaLabel
          .replace(/\s*,\s*open profile menu\s*$/i, "")
          .trim();
        if (cleanedAriaLabel && !/^open profile menu$/i.test(cleanedAriaLabel)) {
          accountText = cleanedAriaLabel;
          break;
        }
      }
      if (accountText) break;
    }

    return accountText;
  }

  // Announce this document so the background registers the canonical conversation it belongs to.
  // document_idle runs at/after tabs.onUpdated "complete", so this - not the load event - is the
  // reliable readiness signal for a conversation tab that is opened or reloaded.
  if (chrome.runtime?.sendMessage) {
    try {
      chrome.runtime.sendMessage({ action: "pageReady" }, () => {
        void chrome.runtime.lastError;
      });
    } catch (announceErr) {
      // Background unavailable (e.g. context invalidated); registration is best-effort.
    }
  }

  // Listen for messages from background.js
  chrome.runtime.onMessage.addListener((request, sender, sendResponse) => {
    if (!request || typeof request !== "object") return false;

    // Op 1: Legacy evidence collector for launch
    if (request.action === "collect_page_evidence") {
      try {
        const conversationId = getCanonicalConversationId();
        if (!conversationId) {
          sendResponse({
            ok: false,
            error: "invalid_conversation_boundary",
            message: "Page is not a canonical existing ChatGPT conversation (e.g. /c/<id>)"
          });
          return true;
        }
        const transcriptText = getRenderedTranscriptText();
        if (!transcriptText) {
          sendResponse({
            ok: false,
            error: "missing_rendered_transcript",
            message: "No rendered conversation transcript turns found in page"
          });
          return true;
        }
        const accountText = getAccountContextText();
        if (!accountText) {
          sendResponse({
            ok: false,
            error: "missing_account_context",
            message: "No account or workspace context evidence found in page"
          });
          return true;
        }
        sendResponse({
          ok: true,
          documentId: DOCUMENT_ID,
          originConversationId: conversationId,
          originConversationUrl: window.location.href.split("#")[0].split("?")[0],
          transcriptText,
          accountText
        });
      } catch (err) {
        sendResponse({ ok: false, error: "evidence_collection_failed", message: String(err) });
      }
      return true;
    }

    // Op 4: Verify Submitted User Message in Transcript (for submitted-observed)
    if (request.action === "verify_submitted_message") {
      try {
        const { receiptMarker, expectedConversationId } = request;
        const currentConvId = getCanonicalConversationId();
        if (!currentConvId || currentConvId !== expectedConversationId) {
          sendResponse({
            ok: false,
            observed: false,
            reason: "conversation_mismatch"
          });
          return true;
        }

        // Look for user messages containing the stable receiptMarker
        const userMessageSelectors = [
          '[data-message-author-role="user"]',
          'main [data-testid^="conversation-turn"]:has([data-message-author-role="user"])',
          'div[data-message-author-role="user"]'
        ];
        let found = null;
        for (const selector of userMessageSelectors) {
          const userTurns = document.querySelectorAll(selector);
          for (const turn of userTurns) {
            const text = (turn.innerText || "").trim();
            if (text.includes(receiptMarker)) {
              // Only accept a real message identity. Conversation-turn data-testid values are
              // render/container identities and may exist for unverified optimistic UI.
              const nestedMessage = typeof turn.querySelector === "function"
                ? turn.querySelector("[data-message-id]")
                : null;
              const rawId = (
                turn.getAttribute("data-message-id") ||
                nestedMessage?.getAttribute("data-message-id") ||
                ""
              ).trim();
              if (rawId) {
                found = { messageId: rawId, text };
                break;
              }
            }
          }
          if (found) break;
        }

        const transcriptText = getRenderedTranscriptText();
        if (found) {
          sendResponse({
            ok: true,
            observed: true,
            observedMessageId: found.messageId,
            transcriptText,
            documentId: DOCUMENT_ID
          });
        } else {
          sendResponse({
            ok: true,
            observed: false,
            transcriptText,
            documentId: DOCUMENT_ID
          });
        }
      } catch (err) {
        sendResponse({
          ok: false,
          observed: false,
          error: String(err)
        });
      }
      return true;
    }

    return false;
  });
})();
