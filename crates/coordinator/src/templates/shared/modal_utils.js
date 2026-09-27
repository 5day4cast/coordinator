const modalOpeners = new WeakMap();

function visibleModalElement(element) {
    return element instanceof HTMLElement && element.isConnected &&
        !element.closest('[hidden], [inert]') && element.getClientRects().length > 0 &&
        getComputedStyle(element).visibility !== 'hidden';
}

function modalFocusables(modal) {
    return Array.from(modal.querySelectorAll('a[href], button, input, select, textarea, [tabindex]'))
        .filter(element => !element.disabled && element.tabIndex >= 0 && visibleModalElement(element));
}

function focusModal(modal) {
    const preferred = modal.querySelector('[data-initial-focus]');
    const target = visibleModalElement(preferred) ? preferred : modalFocusables(modal)[0] || modal;
    target.focus({ preventScroll: true });
}

function openModal($modal, opener = document.activeElement) {
    if (!$modal || $modal.classList.contains('is-active')) return;
    modalOpeners.set($modal, opener);
    // A dialog opened from the phone menu replaces it. Closing returns focus to the menu
    // toggle if its original button is no longer visible.
    const menu = document.getElementById('navToggle');
    if (menu) menu.checked = false;
    $modal.classList.add('is-active');
    document.documentElement.classList.add('is-clipped');
    focusModal($modal);
}

function closeModal($modal) {
    if (!$modal || !$modal.classList.contains('is-active')) return;
    $modal.classList.remove('is-active');
    document.documentElement.classList.toggle('is-clipped', !!document.querySelector('.modal.is-active'));
    // Loaded content goes with the dialog, and with it any refresh it runs.
    $modal.querySelector('[data-clear-on-close]')?.replaceChildren();
    const opener = modalOpeners.get($modal);
    modalOpeners.delete($modal);
    const target = [opener, document.getElementById('navToggle'), document.querySelector('.navbar-brand a[href]')]
        .find(element => visibleModalElement(element) && element !== document.body && !element.disabled);
    target?.focus({ preventScroll: true });
}

function closeAllModals() {
    document.querySelectorAll('.modal.is-active').forEach(closeModal);
}

function setupModalCloseHandlers() {
    document.querySelectorAll('.modal-background, .modal-close, .modal-card-head .delete, .modal-card-foot .button.is-cancel')
        .forEach(($close) => {
            const $target = $close.closest('.modal');
            $close.addEventListener('click', () => closeModal($target));
        });

    document.addEventListener('keydown', (event) => {
        const modal = Array.from(document.querySelectorAll('.modal.is-active')).at(-1);
        if (!modal) return;
        if (event.key === 'Escape') {
            event.preventDefault();
            closeModal(modal);
        } else if (event.key === 'Tab') {
            const focusable = modalFocusables(modal);
            const first = focusable[0];
            const last = focusable.at(-1);
            const current = document.activeElement;
            if (!first) {
                event.preventDefault();
                modal.focus();
            } else if (!focusable.includes(current) || (event.shiftKey ? current === first : current === last)) {
                event.preventDefault();
                (event.shiftKey ? last : first).focus();
            }
        }
    });

    document.addEventListener('focusin', (event) => {
        const modal = Array.from(document.querySelectorAll('.modal.is-active')).at(-1);
        if (modal && !modal.contains(event.target)) focusModal(modal);
    });
}
