// Narrow one-way evidence collector on https://chatgpt.com/*
// Collects canonical existing conversation URL, rendered transcript text, and account evidence.
// Does NOT have access to pairing secrets, native messaging, or execution control.

(() => {
  // Listen for evidence request from extension
  chrome.runtime.onMessage.addListener((request, sender, sendResponse) => {
    if (request && request.action === "collect_page_evidence") {
      try {
        const href = window.location.href;
        const pathname = window.location.pathname;

        // Parse canonical conversation ID: /c/<id> or /g/<gizmo>/c/<id>
        const segments = pathname.split("/").filter(Boolean);
        let conversationId = null;
        if (segments.length === 2 && segments[0] === "c") {
          conversationId = segments[1];
        } else if (segments.length === 4 && segments[0] === "g" && segments[2] === "c") {
          conversationId = segments[3];
        }

        if (!conversationId || conversationId === "new" || conversationId === "chat" || conversationId.includes("new_chat") || conversationId.includes("provisional")) {
          sendResponse({
            ok: false,
            error: "invalid_conversation_boundary",
            message: "Page is not a canonical existing ChatGPT conversation (e.g. /c/<id>)"
          });
          return true;
        }

        // Collect rendered transcript text from conversation turns
        const turnSelectors = [
          'main div[data-testid^="conversation-turn"]',
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
        if (!transcriptText) {
          sendResponse({
            ok: false,
            error: "missing_rendered_transcript",
            message: "No rendered conversation transcript turns found in page"
          });
          return true;
        }

        // Collect account / workspace evidence if available
        const accountNodes = document.querySelectorAll(
          '[data-testid*="user-profile"], [data-testid*="workspace"], button[id*="user-menu"]'
        );
        let accountText = "";
        for (const node of accountNodes) {
          const candidate = (node?.innerText || "").trim();
          if (candidate) {
            accountText = candidate;
            break;
          }
        }
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
          originConversationId: conversationId,
          originConversationUrl: href.split("#")[0].split("?")[0],
          transcriptText,
          accountText
        });
      } catch (err) {
        sendResponse({
          ok: false,
          error: "evidence_collection_failed",
          message: String(err)
        });
      }
    }
    return true;
  });
})();
