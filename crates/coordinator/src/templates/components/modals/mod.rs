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
                    // Tabs for login method - Email (default) and Browser Extension
                    div class="tabs is-centered is-boxed" {
                        ul {
                            li class="is-active" data-target="emailLogin" {
                                a { span { "Email" } }
                            }
                            li data-target="extensionLogin" {
                                a { span { "Browser Extension" } }
                            }
                        }
                    }

                    // Email Login (default)
                    div id="emailLogin" {
                        div class="field" {
                            label class="label" { "Email" }
                            div class="control" {
                                input class="input" type="email" id="loginEmail"
                                      placeholder="you@example.com";
                            }
                        }
                        div class="field" {
                            label class="label" { "Password" }
                            div class="control" {
                                input class="input" type="password" id="loginPassword"
                                      placeholder="Enter your password";
                            }
                        }
                        p class="help is-danger mt-2" id="emailLoginError" {}
                        div class="field mt-4" {
                            div class="control" {
                                button class="button is-info is-fullwidth" id="emailLoginButton" {
                                    "Login"
                                }
                            }
                        }
                        p class="has-text-centered mt-3" {
                            a href="#" id="forgotPasswordLink" class="has-text-grey" {
                                "Forgot password?"
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
                    // Tabs for registration method - Email (default) and Browser Extension
                    div class="tabs is-centered" {
                        ul {
                            li class="is-active" data-target="registerEmail" {
                                a { "Email" }
                            }
                            li data-target="registerExtension" {
                                a { "Browser Extension" }
                            }
                        }
                    }

                    // Email Registration
                    div id="registerEmail" {
                        // Step 1: Email & Password
                        div id="emailRegisterStep1" {
                            div class="field" {
                                label class="label" { "Email" }
                                div class="control" {
                                    input class="input" type="email" id="registerEmailInput"
                                          placeholder="you@example.com";
                                }
                            }
                            div class="field" {
                                label class="label" { "Password" }
                                div class="control" {
                                    input class="input" type="password" id="registerPassword"
                                          placeholder="Choose a strong password (min 8 characters)";
                                }
                            }
                            div class="field" {
                                label class="label" { "Confirm Password" }
                                div class="control" {
                                    input class="input" type="password" id="registerPasswordConfirm"
                                          placeholder="Confirm your password";
                                }
                            }
                            p class="help is-danger mt-2" id="emailRegisterError" {}
                            button class="button is-info is-fullwidth mt-4" id="emailRegisterStep1Button" {
                                "Continue"
                            }
                        }

                        // Step 2: Backup nsec (CRITICAL)
                        div id="emailRegisterStep2" class="is-hidden" {
                            div class="notification is-warning" {
                                strong { "Important: Save Your Recovery Key" }
                                p {
                                    "This key is the ONLY way to recover your account if you forget your password. "
                                    "Without it, your funds will be permanently lost."
                                }
                            }
                            div class="field mt-4" {
                                label class="label" { "Your Recovery Key (nsec)" }
                                div class="control" {
                                    input class="input" type="text" id="emailNsecDisplay" readonly;
                                }
                            }
                            button class="button is-info is-fullwidth mt-2" id="copyEmailNsec" {
                                "Copy to clipboard"
                            }
                            div class="field mt-4" {
                                label class="checkbox" {
                                    input type="checkbox" id="emailNsecSavedCheckbox";
                                    " I have saved my recovery key in a safe place"
                                }
                            }
                            p class="help is-danger mt-2" id="emailRegisterStep2Error" {}
                            button class="button is-success is-fullwidth mt-4"
                                   id="emailRegisterStep2Button" disabled {
                                "Complete Registration"
                            }
                        }

                        // Step 3: Success
                        div id="emailRegisterStep3" class="is-hidden" {
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
                            "Already have an account? Login"
                        }
                    }
                }
            }
        }
    }
}

fn forgot_password_modal() -> Markup {
    html! {
        div id="forgotPasswordModal" class="modal" {
            div class="modal-background" {}
            div class="modal-card" {
                header class="modal-card-head" {
                    p class="modal-card-title" { "Reset Password" }
                    button class="delete" aria-label="close" id="closeForgotPasswordModal" {}
                }
                section class="modal-card-body" {
                    // Step 1: Enter email
                    div id="forgotStep1" {
                        p { "Enter your email to receive a password reset challenge." }
                        div class="field mt-4" {
                            label class="label" { "Email" }
                            div class="control" {
                                input class="input" type="email" id="forgotEmail"
                                      placeholder="you@example.com";
                            }
                        }
                        p class="help is-danger mt-2" id="forgotStep1Error" {}
                        button class="button is-info is-fullwidth mt-4" id="forgotStep1Button" {
                            "Continue"
                        }
                    }

                    // Step 2: Enter nsec and sign challenge
                    div id="forgotStep2" class="is-hidden" {
                        div class="notification is-info is-light" {
                            p { "Enter your recovery key (nsec) to prove account ownership." }
                        }
                        div class="field mt-4" {
                            label class="label" { "Your Recovery Key (nsec)" }
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

                    // Step 3: Set new password
                    div id="forgotStep3" class="is-hidden" {
                        div class="notification is-success is-light" {
                            p { "Ownership verified! Set your new password." }
                        }
                        div class="field mt-4" {
                            label class="label" { "New Password" }
                            div class="control" {
                                input class="input" type="password" id="forgotNewPassword"
                                      placeholder="Choose a new password (min 8 characters)";
                            }
                        }
                        div class="field" {
                            label class="label" { "Confirm New Password" }
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

                    // Back to login link
                    p class="has-text-centered mt-5" {
                        a href="#" id="backToLoginFromForgot" class="has-text-info" {
                            "Back to Login"
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
