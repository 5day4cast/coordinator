use maud::{html, Markup};

pub fn auth_modals() -> Markup {
    html! {
        // Login Modal
        (login_modal())

        // Registration Modal
        (register_modal())

        // Forgot Password Modal
        (forgot_password_modal())

        // Payment Modal (for entry ticket payments)
        (payment_modal())

        // Payout Modal (for submitting lightning invoices)
        (payout_modal())

        // Entry Score Modal (for viewing entry details)
        (entry_score_modal())
    }
}

/// The rule `validatePasswordStrength` in modals.js enforces.
const PASSWORD_RULE: &str = "At least 10 characters, with an upper-case letter, a lower-case letter, a number and a symbol.";

/// Log-in and sign-up both offer a password account or a Nostr extension.
fn auth_tabs(username_panel: &str, extension_panel: &str) -> Markup {
    html! {
        div class="tabs is-centered is-boxed auth-tabs" {
            ul role="tablist" aria-label="Sign-in method" {
                li class="is-active" data-target=(username_panel) role="presentation" {
                    a id=(format!("{username_panel}Tab")) href=(format!("#{username_panel}"))
                        role="tab" aria-controls=(username_panel) aria-selected="true" tabindex="0" {
                        "Username and password"
                    }
                }
                li data-target=(extension_panel) role="presentation" {
                    a id=(format!("{extension_panel}Tab")) href=(format!("#{extension_panel}"))
                        role="tab" aria-controls=(extension_panel) aria-selected="false" tabindex="-1" {
                        "Nostr extension"
                    }
                }
            }
        }
    }
}

/// Keys are held only in this tab's memory, so say what that means.
fn session_note() -> Markup {
    html! {
        p class="help session-note mb-3" {
            "For safety your key stays in this tab's memory only: reloading the page or opening a new tab signs you out."
        }
    }
}

fn extension_note() -> Markup {
    html! {
        p class="mb-4" {
            "Use a Nostr signer extension (NIP-07) such as Alby or nos2x. "
            "Your key stays in the extension; it signs for this site when asked."
        }
    }
}

fn login_modal() -> Markup {
    html! {
        div id="loginModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="loginModalTitle" tabindex="-1" {
            div class="modal-background" {}
            div class="modal-card" {
                header class="modal-card-head" {
                    p id="loginModalTitle" class="modal-card-title" { "Log in" }
                    button id="closeLoginModal" class="delete" aria-label="close" {}
                }
                section class="modal-card-body" {
                    (session_note())
                    (auth_tabs("usernameLogin", "extensionLogin"))

                    div id="usernameLogin" role="tabpanel" aria-labelledby="usernameLoginTab" {
                        div class="field" {
                            label class="label" for="loginUsername" { "Username" }
                            div class="control" {
                                input class="input" type="text" id="loginUsername" data-initial-focus
                                      placeholder="username";
                            }
                        }
                        div class="field" {
                            label class="label" for="loginPassword" { "Password" }
                            div class="control" {
                                input class="input" type="password" id="loginPassword"
                                      placeholder="password";
                            }
                        }
                        p class="help is-danger mt-2" id="usernameLoginError" {}
                        div class="field mt-4" {
                            div class="control" {
                                button class="button is-info is-fullwidth" id="usernameLoginButton" {
                                    "Log in"
                                }
                            }
                        }
                        p class="has-text-centered mt-3" {
                            a href="#" id="forgotPasswordLink" class="has-text-grey" {
                                "Forgot password?"
                            }
                        }
                    }

                    div id="extensionLogin" role="tabpanel" aria-labelledby="extensionLoginTab" class="is-hidden" {
                        (extension_note())
                        div class="field" {
                            div class="control" {
                                button class="button is-info is-fullwidth" id="extensionLoginButton" {
                                    "Connect with Extension"
                                }
                            }
                            p class="help is-danger mt-2" id="extensionLoginError" {}
                        }
                    }

                    p class="has-text-centered mt-5" {
                        a href="#" id="showRegisterButton" class="has-text-info" {
                            "Need an account? Sign up"
                        }
                    }
                }
            }
        }
    }
}

/// Where winnings are paid. Required at signup so payouts need no further
/// input from the winner.
fn lightning_address_field(id: &str) -> Markup {
    html! {
        div class="field" {
            label class="label" for=(id) { "Lightning Address" }
            div class="control" {
                input class="input" type="text" id=(id) placeholder="you@cash.app"
                      autocomplete="off" spellcheck="false";
            }
            p class="help" {
                "Winnings are paid here automatically. Cash App users: your $cashtag followed by @cash.app."
            }
        }
    }
}

fn register_modal() -> Markup {
    html! {
        div id="registerModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="registerModalTitle" tabindex="-1" {
            div class="modal-background" {}
            div class="modal-card" {
                header class="modal-card-head" {
                    p id="registerModalTitle" class="modal-card-title" { "Sign up" }
                    button id="closeResisterModal" class="delete" aria-label="close" {}
                }
                section class="modal-card-body" {
                    (session_note())
                    (auth_tabs("registerUsername", "registerExtension"))

                    div id="registerUsername" role="tabpanel" aria-labelledby="registerUsernameTab" {
                        div id="usernameRegisterStep1" {
                            div class="field" {
                                label class="label" for="registerUsernameInput" { "Username" }
                                div class="control" {
                                    input class="input" type="text" id="registerUsernameInput" data-initial-focus
                                          placeholder="username";
                                }
                                p class="help" { "3-32 characters, letters, numbers, underscores, hyphens" }
                            }
                            div class="field" {
                                label class="label" for="registerPassword" { "Password" }
                                div class="control" {
                                    input class="input" type="password" id="registerPassword"
                                          placeholder="Choose a strong password" autocomplete="new-password";
                                }
                                p class="help" { (PASSWORD_RULE) }
                            }
                            div class="field" {
                                label class="label" for="registerPasswordConfirm" { "Confirm Password" }
                                div class="control" {
                                    input class="input" type="password" id="registerPasswordConfirm"
                                          placeholder="Confirm your password";
                                }
                            }
                            (lightning_address_field("registerLightningAddress"))
                            p class="help is-danger mt-2" id="usernameRegisterError" {}
                            button class="button is-info is-fullwidth mt-4" id="usernameRegisterStep1Button" {
                                "Continue"
                            }
                        }

                        div id="usernameRegisterStep2" class="is-hidden" {
                            div class="notification is-warning" {
                                strong { "Important: Save Your Recovery Key" }
                                p {
                                    "This key is the ONLY way to recover your account if you forget your password. "
                                    "Without it, your funds will be permanently lost."
                                }
                            }
                            div class="field mt-4" {
                                label class="label" for="usernameNsecDisplay" { "Your Recovery Key (nsec)" }
                                div class="control" {
                                    input class="input" type="text" id="usernameNsecDisplay" readonly;
                                }
                            }
                            button class="button is-info is-fullwidth mt-2" id="copyUsernameNsec" {
                                "Copy to clipboard"
                            }
                            div class="field mt-4" {
                                label class="checkbox" {
                                    input type="checkbox" id="usernameNsecSavedCheckbox";
                                    " I have saved my recovery key in a safe place"
                                }
                            }
                            p class="help is-danger mt-2" id="usernameRegisterStep2Error" {}
                            button class="button is-success is-fullwidth mt-4"
                                   id="usernameRegisterStep2Button" disabled {
                                "Complete Registration"
                            }
                        }

                        div id="usernameRegisterStep3" class="is-hidden" {
                            div class="has-text-centered" {
                                h2 class="title" { "Welcome!" }
                                p class="subtitle" { "Your account has been created successfully." }
                            }
                        }
                    }

                    div id="registerExtension" role="tabpanel" aria-labelledby="registerExtensionTab" class="is-hidden" {
                        (extension_note())
                        (lightning_address_field("extensionLightningAddress"))
                        div class="field" {
                            div class="control" {
                                button class="button is-info is-fullwidth" id="extensionRegisterButton" {
                                    "Register with Extension"
                                }
                            }
                            p class="help is-danger mt-2" id="extensionRegisterError" {}
                        }
                    }

                    p class="has-text-centered mt-5" {
                        a href="#" id="goToLoginButton" class="has-text-info" {
                            "Already have an account? Log in"
                        }
                    }
                }
            }
        }
    }
}

fn forgot_password_modal() -> Markup {
    html! {
        div id="forgotPasswordModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="forgotPasswordModalTitle" tabindex="-1" {
            div class="modal-background" {}
            div class="modal-card" {
                header class="modal-card-head" {
                    p id="forgotPasswordModalTitle" class="modal-card-title" { "Reset Password" }
                    button class="delete" aria-label="close" id="closeForgotPasswordModal" {}
                }
                section class="modal-card-body" {
                    div id="forgotStep1" {
                        p { "Enter your username to start the password reset process." }
                        div class="field mt-4" {
                            label class="label" for="forgotUsername" { "Username" }
                            div class="control" {
                                input class="input" type="text" id="forgotUsername" data-initial-focus
                                      placeholder="username";
                            }
                        }
                        p class="help is-danger mt-2" id="forgotStep1Error" {}
                        button class="button is-info is-fullwidth mt-4" id="forgotStep1Button" {
                            "Continue"
                        }
                    }

                    div id="forgotStep2" class="is-hidden" {
                        div class="notification is-info is-light" {
                            p { "Enter your recovery key (nsec) to prove account ownership." }
                        }
                        div class="field mt-4" {
                            label class="label" for="forgotNsec" { "Your Recovery Key (nsec)" }
                            div class="control" {
                                input class="input" type="password" id="forgotNsec"
                                      placeholder="nsec1...";
                            }
                        }
                        p class="help is-danger mt-2" id="forgotStep2Error" {}
                        button class="button is-info is-fullwidth mt-4" id="forgotStep2Button" {
                            "Verify Ownership"
                        }
                    }

                    div id="forgotStep3" class="is-hidden" {
                        div class="notification is-success is-light" {
                            p { "Ownership verified! Set your new password." }
                        }
                        div class="field mt-4" {
                            label class="label" for="forgotNewPassword" { "New Password" }
                            div class="control" {
                                input class="input" type="password" id="forgotNewPassword"
                                      placeholder="Choose a new password" autocomplete="new-password";
                            }
                            p class="help" { (PASSWORD_RULE) }
                        }
                        div class="field" {
                            label class="label" for="forgotNewPasswordConfirm" { "Confirm New Password" }
                            div class="control" {
                                input class="input" type="password" id="forgotNewPasswordConfirm"
                                      placeholder="Confirm your new password";
                            }
                        }
                        p class="help is-danger mt-2" id="forgotStep3Error" {}
                        button class="button is-success is-fullwidth mt-4" id="forgotStep3Button" {
                            "Reset Password"
                        }
                    }

                    p class="has-text-centered mt-5" {
                        a href="#" id="backToLoginFromForgot" class="has-text-info" {
                            "Back to log in"
                        }
                    }
                }
            }
        }
    }
}

fn payment_modal() -> Markup {
    html! {
        div id="ticketPaymentModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="ticketPaymentModalTitle" tabindex="-1" {
            div class="modal-background" {}
            div class="modal-content" {
                div class="box" {
                    h3 id="ticketPaymentModalTitle" class="title is-4" { "Entry Ticket Payment" }
                    div class="content" {
                        p id="ticketPaymentAmount" { "Please pay the lightning invoice to enter the competition:" }

                        // QR code; tapping it copies the invoice (entry_form.js).
                        div id="qrContainer" class="has-text-centered mb-2" {}
                        p class="help has-text-centered mb-3" { "Tap the QR code to copy the invoice" }

                        // Links that hand the invoice to a wallet app on this
                        // device; entry_form.js fills in their hrefs.
                        div id="walletLinks" class="buttons is-centered mb-4" {
                            a id="walletLinkLightning" class="button is-link is-light" { "Open in wallet" }
                            a id="walletLinkZeus" class="button is-light" { "Pay with Zeus" }
                            a id="walletLinkCashApp" class="button is-light" target="_blank" rel="noopener noreferrer" { "Pay with Cash App" }
                        }

                        div class="field" {
                            label class="label" for="paymentRequest" { "Payment Request (click to copy)" }
                            div class="control" {
                                textarea class="textarea" id="paymentRequest" readonly {}
                            }
                            p class="help is-success is-hidden" id="copyFeedback" {
                                "✓ Copied to clipboard"
                            }
                        }

                        div id="paymentStatus" class="mt-4" {
                            p { "Waiting for payment..." }
                            progress class="progress is-info" max="100" {}
                        }
                        div id="ticketPaymentError" class="notification is-danger is-hidden" {}
                    }
                }
            }
            button class="modal-close is-large" aria-label="close" {}
        }
    }
}

fn payout_modal() -> Markup {
    html! {
        div id="payoutModal" class="modal" role="dialog" aria-modal="true" aria-labelledby="payoutModalTitle" tabindex="-1" {
            div class="modal-background" {}
            div class="modal-content" {
                div class="box" {
                    h3 id="payoutModalTitle" class="title is-4" { "Submit Lightning Invoice" }
                    p id="payoutAmountSummary" class="mb-3" {}
                    div class="field" {
                        label class="label" for="lightningInvoice" { "Lightning Invoice" }
                        div class="control" {
                            textarea class="textarea" id="lightningInvoice"
                                     data-initial-focus aria-describedby="payoutAmountSummary payoutModalError"
                                     placeholder="Enter your Lightning invoice here..." {}
                        }
                    }
                    div class="field is-grouped" {
                        div class="control" {
                            button class="button is-primary" id="submitPayoutInvoice" { "Submit" }
                        }
                        div class="control" {
                            button class="button is-light" id="cancelPayoutModal" { "Cancel" }
                        }
                    }
                    div id="payoutModalError" class="notification is-danger hidden" role="alert" {}
                }
            }
            button class="modal-close is-large" aria-label="close" {}
        }
    }
}

fn entry_score_modal() -> Markup {
    html! {
        div id="entryScore" class="modal" role="dialog" aria-modal="true" aria-label="Entry picks" tabindex="-1" {
            div class="modal-background" {}
            div class="modal-content" {
                div class="box" {
                    // Emptied on close, which also stops a live entry's refresh.
                    div id="entryValues" data-clear-on-close {}
                }
            }
            button class="modal-close is-large" aria-label="close" {}
        }
    }
}
