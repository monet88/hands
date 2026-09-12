// Narrow one-way evidence collector on https://chatgpt.com/*
// Collects canonical existing conversation URL, rendered transcript text, and account evidence.
// Does NOT have access to pairing secrets, native messaging, or execution control.

(() => {
  // Stable per-document identity generated on script load
  const DOCUMENT_ID = "doc_" + (globalThis.crypto?.randomUUID ? globalThis.crypto.randomUUID().replace(/-/g, "").slice(0, 16) : Math.random().toString(36).slice(2, 18));
  const SCRIPT_LOADED_URL = window.location.href.split("#")[0].split("?")[0];

  // Track one-time grant consumption by attemptId in this live document
  const consumedGrantAttemptIds = new Set();

  function isButtonEnabled(btn) {
    if (!btn) return false;
    if (btn.disabled) return false;
    if (btn.getAttribute && btn.getAttribute("aria-disabled") === "true") return false;
    return true;
  }
  function clearComposer(composer, expectedText) {
    if (!composer) return;
    try {
      const currentText = (composer.tagName === "TEXTAREA" ? (composer.value || "") : (composer.innerText || "")).trim();
      // Only clear bridge-authored text if the composer still exactly equals the bridge continuation payload
      // If the composer differs (user-edited/tampered), preserve it untouched
      if (expectedText === undefined || currentText !== expectedText.trim()) {
        return;
      }
      if (composer.tagName === "TEXTAREA") {
        composer.value = "";
        composer.dispatchEvent(new Event("input", { bubbles: true }));
        composer.dispatchEvent(new Event("change", { bubbles: true }));
      } else if (composer.isContentEditable) {
        composer.innerText = "";
        try {
          composer.dispatchEvent(new InputEvent("input", { bubbles: true, cancelable: true, inputType: "deleteContentBackward" }));
        } catch {}
      }
    } catch {}
  }
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

  function checkReadinessGuards(expectedConversationId, expectedConversationUrl) {
    // 1. Navigation / Route change check
    const currentUrl = window.location.href.split("#")[0].split("?")[0];
    if (currentUrl !== SCRIPT_LOADED_URL) {
      return { ready: false, reason: "navigation_invalidated", message: "Document URL changed since script load" };
    }
    const currentConvId = getCanonicalConversationId();
    if (!currentConvId || currentConvId !== expectedConversationId) {
      return { ready: false, reason: "conversation_mismatch", message: "Page is not the expected canonical conversation" };
    }
    if (expectedConversationUrl && currentUrl !== expectedConversationUrl && !currentUrl.endsWith(`/c/${expectedConversationId}`)) {
      return { ready: false, reason: "conversation_mismatch", message: "Page URL does not match expected conversation" };
    }
    // 2. Loading / Login / Error page check
    if (document.querySelector('[data-testid="login-button"], form[action*="login"], .auth-error, [data-testid="error-banner"]')) {
      return { ready: false, reason: "login_or_error_page", message: "Page shows login or error state" };
    }
    if (document.readyState === "loading") {
      return { ready: false, reason: "document_loading", message: "Document is still loading" };
    }

    // 3. Active generation check (never stop generation)
    const stopButton = document.querySelector(
      'button[data-testid="stop-button"], button[aria-label*="Stop generating"], button[data-testid="fruitjuice-stop-button"]'
    );
    if (stopButton) {
      return { ready: false, reason: "active_generation", message: "ChatGPT is currently generating a response" };
    }

    // 4. Draft preservation check (never overwrite user draft)
    const promptTextarea = document.querySelector(
      '#prompt-textarea, textarea[data-id="root"], div[contenteditable="true"]#prompt-textarea'
    );
    if (promptTextarea) {
      const draftText = (promptTextarea.value !== undefined ? promptTextarea.value : promptTextarea.innerText || "").trim();
      if (draftText.length > 0) {
        return { ready: false, reason: "unrelated_draft_present", message: "User draft present in composer; preserving draft without overwrite" };
      }
    } else {
      return { ready: false, reason: "composer_not_found", message: "Prompt composer element not found" };
    }

    // 5. Context evidence
    const transcriptText = getRenderedTranscriptText();
    if (!transcriptText) {
      return { ready: false, reason: "missing_transcript", message: "No conversation transcript rendered" };
    }
    const accountText = getAccountContextText();
    if (!accountText) {
      return { ready: false, reason: "missing_account_context", message: "No account/workspace context found" };
    }

    return {
      ready: true,
      documentId: DOCUMENT_ID,
      conversationId: currentConvId,
      currentUrl,
      transcriptText,
      accountText
    };
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

    // Op 2: Check delivery readiness (pre-grant check)
    if (request.action === "check_delivery_readiness") {
      try {
        const readiness = checkReadinessGuards(
          request.expectedConversationId,
          request.expectedConversationUrl
        );
        sendResponse({
          ok: readiness.ready,
          readiness,
          documentId: DOCUMENT_ID
        });
      } catch (err) {
        sendResponse({ ok: false, error: "readiness_check_failed", message: String(err) });
      }
      return true;
    }

    // Op 3: Consume Grant and Synchronous Guard-and-Click
    if (request.action === "consume_grant_and_dispatch") {
      (async () => {
        let composer = null;
        const continuationText = request?.continuationText;
        try {
          const { attemptId, expectedDocumentId, expectedConversationId, expectedConversationUrl, expectedAccountText, receiptMarker } = request;

        // Document Identity Check: grant is bound to this specific document
        if (expectedDocumentId !== DOCUMENT_ID) {
          sendResponse({
            ok: false,
            clicked: false,
            reason: "document_identity_mismatch",
            message: "Grant was issued for a different document identity"
          });
          return true;
        }

        // One-time grant consumption guard: reject replay of the exact same attemptId (Finding 7)
        if (consumedGrantAttemptIds.has(attemptId)) {
          sendResponse({
            ok: false,
            clicked: false,
            reason: "grant_already_consumed",
            message: "Grant attempt already consumed by this live document; duplicate click prevented"
          });
          return true;
        }

        // Mark this attemptId consumed
        consumedGrantAttemptIds.add(attemptId);
        // Synchronous final readiness guard (zero asynchronous gap!)
        const finalReadiness = checkReadinessGuards(expectedConversationId, expectedConversationUrl);
        if (!finalReadiness.ready) {
          sendResponse({
            ok: false,
            clicked: false,
            reason: finalReadiness.reason,
            message: finalReadiness.message
          });
          return true;
        }

        // Locate composer
        composer = document.querySelector(
          '#prompt-textarea, textarea[data-id="root"], div[contenteditable="true"]#prompt-textarea'
        );
        if (!composer) {
          sendResponse({
            ok: false,
            clicked: false,
            reason: "composer_elements_missing",
            message: "Composer element missing at dispatch moment"
          });
          return true;
        }

        // Populate continuation payload into composer
        if (composer.tagName === "TEXTAREA") {
          composer.value = continuationText;
          composer.dispatchEvent(new Event("input", { bubbles: true }));
          composer.dispatchEvent(new Event("change", { bubbles: true }));
        } else if (composer.isContentEditable) {
          composer.focus();
          try {
            composer.dispatchEvent(new InputEvent("beforeinput", { bubbles: true, cancelable: true, inputType: "insertText", data: continuationText }));
          } catch {}
          let inserted = false;
          try {
            inserted = document.execCommand("insertText", false, continuationText);
          } catch {}
          if (!inserted || !composer.innerText.trim()) {
            composer.innerText = continuationText;
          }
          try {
            composer.dispatchEvent(new InputEvent("input", { bubbles: true, cancelable: true, inputType: "insertText", data: continuationText }));
          } catch {}
        }

        // Locate send button (mounted/enabled upon text input) (Finding 8)
        const sendBtnSelector = 'button[data-testid="send-button"], button[aria-label*="Send prompt" i], button[data-testid="fruitjuice-send-button"], #composer-submit-button';
        let sendBtn = document.querySelector(sendBtnSelector);
        if (!isButtonEnabled(sendBtn)) {
          for (let i = 0; i < 10; i++) {
            await new Promise((res) => setTimeout(res, 60));
            sendBtn = document.querySelector(sendBtnSelector);
            if (isButtonEnabled(sendBtn)) break;
          }
        }

        if (!isButtonEnabled(sendBtn)) {
          clearComposer(composer, continuationText);
          sendResponse({
            ok: false,
            clicked: false,
            reason: "send_button_disabled",
            message: "Send button missing or disabled after text insertion"
          });
          return true;
        }

        // Finding 6: Immediately before click revalidate document/conversation, generation/readiness,
        // composer still contains exact continuation payload, and the actual button is valid and enabled
        if (expectedDocumentId !== DOCUMENT_ID) {
          clearComposer(composer, continuationText);
          sendResponse({
            ok: false,
            clicked: false,
            reason: "document_identity_mismatch",
            message: "Document identity mismatch at click moment"
          });
          return true;
        }

        const currentUrl = window.location.href.split("#")[0].split("?")[0];
        if (currentUrl !== SCRIPT_LOADED_URL) {
          clearComposer(composer, continuationText);
          sendResponse({
            ok: false,
            clicked: false,
            reason: "navigation_invalidated",
            message: "Document URL changed since script load"
          });
          return true;
        }
        const currentConvId = getCanonicalConversationId();
        if (!currentConvId || currentConvId !== expectedConversationId) {
          clearComposer(composer, continuationText);
          sendResponse({
            ok: false,
            clicked: false,
            reason: "conversation_mismatch",
            message: "Page is not the expected canonical conversation at click moment"
          });
          return true;
        }

        if (expectedConversationUrl && currentUrl !== expectedConversationUrl && !currentUrl.endsWith(`/c/${expectedConversationId}`)) {
          clearComposer(composer, continuationText);
          sendResponse({
            ok: false,
            clicked: false,
            reason: "conversation_mismatch",
            message: "Page URL does not match expected conversation at click moment"
          });
          return true;
        }

        if (document.querySelector('[data-testid="login-button"], form[action*="login"], .auth-error, [data-testid="error-banner"]')) {
          clearComposer(composer, continuationText);
          sendResponse({
            ok: false,
            clicked: false,
            reason: "login_or_error_page",
            message: "Page shows login or error state at click moment"
          });
          return true;
        }

        const stopBtn = document.querySelector(
          'button[data-testid="stop-button"], button[aria-label*="Stop generating"], button[data-testid="fruitjuice-stop-button"]'
        );
        if (stopBtn) {
          clearComposer(composer, continuationText);
          sendResponse({
            ok: false,
            clicked: false,
            reason: "active_generation",
            message: "ChatGPT is currently generating a response at click moment"
          });
          return true;
        }

        // Revalidate the same account/workspace context immediately before click.
        // Keep this synchronous: hashing here would introduce a fresh async gap after the native grant.
        if (expectedAccountText) {
          const currentAccountText = getAccountContextText();
          if (!currentAccountText || currentAccountText !== expectedAccountText) {
            clearComposer(composer, continuationText);
            sendResponse({
              ok: false,
              clicked: false,
              reason: "account_context_mismatch",
              message: "Account/workspace context changed after dispatch grant"
            });
            return true;
          }
        }

        // Re-query the current attached composer after the async send-button wait (P2 3995737142)
        const currentComposer = document.querySelector(
          '#prompt-textarea, textarea[data-id="root"], div[contenteditable="true"]#prompt-textarea'
        );
        const isComposerAttached = currentComposer && (document.contains ? document.contains(currentComposer) : (document.body ? document.body.contains(currentComposer) : true));
        if (!currentComposer || !isComposerAttached || currentComposer !== composer) {
          clearComposer(currentComposer, continuationText);
          clearComposer(composer, continuationText);
          sendResponse({
            ok: false,
            clicked: false,
            reason: "composer_detached_or_replaced",
            message: "Composer was detached or replaced during send button wait"
          });
          return true;
        }

        const currentComposerText = (currentComposer.tagName === "TEXTAREA" ? (currentComposer.value || "") : (currentComposer.innerText || "")).trim();
        if (!currentComposerText || currentComposerText !== continuationText.trim()) {
          clearComposer(currentComposer, continuationText);
          sendResponse({
            ok: false,
            clicked: false,
            reason: "composer_content_tampered",
            message: "Composer content does not match continuation payload at click moment"
          });
          return true;
        }

        const isAttached = document.contains ? document.contains(sendBtn) : (document.body ? document.body.contains(sendBtn) : true);
        if (!isAttached || !isButtonEnabled(sendBtn)) {
          clearComposer(composer, continuationText);
          sendResponse({
            ok: false,
            clicked: false,
            reason: "send_button_invalid",
            message: "Send button is no longer valid, attached, or enabled at click moment"
          });
          return true;
        }

        sendBtn.click();
        sendResponse({
          ok: true,
          clicked: true,
          documentId: DOCUMENT_ID,
          attemptId,
          receiptMarker
        });
        } catch (err) {
          clearComposer(composer, continuationText);
          sendResponse({
            ok: false,
            clicked: false,
            reason: "click_execution_failed",
            message: String(err)
          });
        }
      })();
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
