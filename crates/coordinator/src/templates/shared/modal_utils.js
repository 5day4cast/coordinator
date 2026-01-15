function openModal($modal) {
    if (!$modal) return;
    $modal.classList.add('is-active');
    document.documentElement.classList.add('is-clipped');
}

function closeModal($modal) {
    if (!$modal) return;
    $modal.classList.remove('is-active');
    document.documentElement.classList.remove('is-clipped');
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
        if (event.key === 'Escape') closeAllModals();
    });
}

window.openModal = openModal;
window.closeModal = closeModal;
window.closeAllModals = closeAllModals;
window.setupModalCloseHandlers = setupModalCloseHandlers;
