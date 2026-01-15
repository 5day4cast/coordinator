use maud::{html, Markup};

/// Authentication modals (login and registration)
///
/// These modals are controlled by JS (crypto_bridge.js) since they involve
/// WASM operations for Nostr key generation and wallet initialization.
pub fn auth_modals() -> Markup {
    html! {
        // Login Modal
        (login_modal())

        // Registration Modal
        (register_modal())

        // Payment Modal (for entry ticket payments)
        (payment_modal())

        // Payout Modal (for submitting lightning invoices)
        (payout_modal())

        // Entry Score Modal (for viewing entry details)
        (entry_score_modal())
    }
}

fn login_modal() -> Markup {
    html! {
        div id="loginModal" class="modal" {
            div class="modal-background" {}
            div class="modal-card" {
                header class="modal-card-head" {
                    p class="modal-card-title" { "Welcome Back" }
                    button id="closeLoginModal" class="delete" aria-label="close" {}
                }
                section class="modal-card-body" {
                    // Tabs for login method
                    div class="tabs is-centered is-boxed" {
                        ul {
                            li class="is-active" data-target="privateKeyLogin" {
                                a { span { "Private Key" } }
                            }
                            li data-target="extensionLogin" {
                                a { span { "Browser Extension" } }
                            }
                        }
                    }

                    // Private Key Login
                    div id="privateKeyLogin" {
                        div class="field" {
                            div class="control" {
                                input class="input is-medium" type="password"
                                      id="loginPrivateKey"
                                      placeholder="Enter your private key";
                            }
                            p class="help is-danger mt-2" id="privateKeyError" {}
                        }
                        div class="field mt-5" {
                            div class="control" {
                                button class="button is-info is-fullwidth" id="loginButton" {
                                    "Login"
                                }
                            }
                        }
                    }

                    // Extension Login
                    div id="extensionLogin" class="is-hidden" {
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

fn register_modal() -> Markup {
    html! {
        div id="registerModal" class="modal" {
            div class="modal-background" {}
            div class="modal-card" {
                header class="modal-card-head" {
                    p class="modal-card-title" { "Create Account" }
                    button id="closeResisterModal" class="delete" aria-label="close" {}
                }
                section class="modal-card-body" {
                    // Tabs for registration method
                    div class="tabs is-centered" {
                        ul {
                            li class="is-active" data-target="registerPrivateKey" {
                                a { "Private Key" }
                            }
                            li data-target="registerExtension" {
                                a { "Browser Extension" }
                            }
                        }
                    }

                    // Private Key Registration
                    div id="registerPrivateKey" {
                        div id="registerStep1" {
                            p {
                                "Copy and put this private key in a safe place. "
                                "Nostr accounts do not have password reset. "
                                "Without the private key, you will not be able to access your account."
                            }
                            div class="field mt-4" {
                                div class="control" {
                                    input class="input" type="text" id="privateKeyDisplay" readonly;
                                }
                            }
                            button class="button is-info is-fullwidth mt-4" id="copyPrivateKey" {
                                "Copy to clipboard"
                            }
                            div class="field mt-4" {
                                label class="checkbox" {
                                    input type="checkbox" id="privateKeySavedCheckbox";
                                    " I have put my private key in a safe place"
                                }
                            }
                            button class="button is-info is-fullwidth mt-4"
                                   id="registerStep1Button" disabled {
                                "Next"
                            }
                        }
                        div id="registerStep2" class="is-hidden" {
                            div class="has-text-centered" {
                                h2 class="title" { "Welcome!" }
                                p class="subtitle" { "Your account has been created successfully." }
                            }
                        }
                    }

                    // Extension Registration
                    div id="registerExtension" class="is-hidden" {
                        p class="mb-4" {
                            "Register a new account using your Nostr browser extension."
                        }
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
                            "Try Login?"
                        }
                    }
                }
            }
        }
    }
}

fn payment_modal() -> Markup {
    html! {
        div id="ticketPaymentModal" class="modal" {
            div class="modal-background" {}
            div class="modal-content" {
                div class="box" {
                    h3 class="title is-4" { "Entry Ticket Payment" }
                    div class="content" {
                        p { "Please pay the lightning invoice to enter the competition:" }

                        // QR Code container
                        div id="qrContainer" class="has-text-centered mb-4" {}

                        div class="field" {
                            label class="label" { "Payment Request (click to copy)" }
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
        div id="payoutModal" class="modal" {
            div class="modal-background" {}
            div class="modal-content" {
                div class="box" {
                    h3 class="title is-4" { "Submit Lightning Invoice" }
                    div class="field" {
                        label class="label" { "Lightning Invoice" }
                        div class="control" {
                            textarea class="textarea" id="lightningInvoice"
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
                    div id="payoutModalError" class="notification is-danger hidden" {}
                }
            }
            button class="modal-close is-large" aria-label="close" {}
        }
    }
}

fn entry_score_modal() -> Markup {
    html! {
        div id="entryScore" class="modal" {
            div class="modal-background" {}
            div class="modal-content" {
                div class="box" {
                    div id="entryValues" {}
                }
            }
            button class="modal-close is-large" aria-label="close" {}
        }
    }
}
