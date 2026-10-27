//! The role each token plays in its simple command.
//!
//! Token kinds say *what a token is* (`Word`, `Redirect`, `Operator`); they do
//! not say *what it is doing*. A consumer that wants "the command word" or
//! "is this an assignment" otherwise has to re-derive simple-command
//! boundaries from token kinds, reserved-word tables and text, and every
//! consumer gets it slightly differently. [`classify_tokens`] does that work
//! once, from the grammar's rules.
//!
//! The roles describe one simple command at a time. An operator, a newline or
//! a reserved word is a [`TokenRole::Boundary`]: it ends the command, so the
//! next word is either an assignment or the next command word. A redirection
//! changes nothing about command position (`>out cmd` still has `cmd` in
//! command position), but a target written separately is
//! [`TokenRole::RedirectTarget`] and is not an argument.
//!
//! Roles are positional, not intrinsic to a token, so they are computed over a
//! slice: `classify_tokens(&tokens, src)` returns one role per token, in the
//! same order. `src` must be the buffer the token spans index into.

use crate::lexer::{Token, TokenKind, assignment_value_start};

/// The grammatical role a token plays in its simple command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenRole {
    /// Layout or a comment: no grammatical role.
    Filler,
    /// A token that ends the current simple command: an operator, a newline,
    /// or a reserved word. Assignments and a command word may follow.
    Boundary,
    /// `NAME=value` (or `NAME+=value`, `NAME[sub]=value`) in the assignment
    /// prefix of a simple command.
    Assignment,
    /// The command word: the first word after the assignments.
    CommandWord,
    /// A word after the command word.
    Argument,
    /// A redirection operator. A target written hard against it is part of the
    /// same token; [`Token::redirect_target`] returns that span.
    RedirectOperator,
    /// The word that names a redirection's target.
    RedirectTarget,
}

impl TokenRole {
    /// Whether this role names a word (assignment, command word or argument).
    pub fn is_word(self) -> bool {
        matches!(
            self,
            TokenRole::Assignment | TokenRole::CommandWord | TokenRole::Argument
        )
    }

    /// Whether this role names the command word.
    pub fn is_command_word(self) -> bool {
        self == TokenRole::CommandWord
    }

    /// Whether this role is part of a redirection.
    pub fn is_redirect(self) -> bool {
        matches!(
            self,
            TokenRole::RedirectOperator | TokenRole::RedirectTarget
        )
    }
}

/// Classify every token into the role it plays in its simple command.
///
/// The result has one entry per token, in the same order. `src` must be the
/// buffer the tokens' spans index into.
pub fn classify_tokens(tokens: &[Token], src: &[u8]) -> Vec<TokenRole> {
    let mut roles = Vec::with_capacity(tokens.len());
    // Whether the current simple command still needs its command word, i.e.
    // whether assignments and the command word may still appear.
    let mut needs_command = true;
    // A redirection operator with no target written against it: the next word
    // is its target.
    let mut wants_target = false;
    // Inside a `[[ … ]]`, where `]]` closes and is a boundary.
    let mut in_test = false;

    for token in tokens {
        let text = token.text(src);
        let role = match token.kind {
            TokenKind::Whitespace | TokenKind::Comment => TokenRole::Filler,
            TokenKind::HereDocBody | TokenKind::HereDocDelimiter => TokenRole::Filler,
            TokenKind::Newline => {
                needs_command = true;
                wants_target = false;
                TokenRole::Boundary
            }
            TokenKind::Operator => {
                needs_command = true;
                wants_target = false;
                TokenRole::Boundary
            }
            TokenKind::ArithCommand => {
                needs_command = true;
                wants_target = false;
                TokenRole::Boundary
            }
            TokenKind::Redirect => {
                wants_target = token.redirect_target(src).is_none();
                TokenRole::RedirectOperator
            }
            TokenKind::Keyword | TokenKind::Word | TokenKind::WordUnterminated => {
                if text == b"]]" && in_test {
                    in_test = false;
                    needs_command = true;
                    wants_target = false;
                    TokenRole::Boundary
                } else if token.kind == TokenKind::Keyword {
                    if text == b"[[" {
                        in_test = true;
                    }
                    needs_command = true;
                    wants_target = false;
                    TokenRole::Boundary
                } else if wants_target {
                    wants_target = false;
                    TokenRole::RedirectTarget
                } else if needs_command && assignment_value_start(src, &token.span).is_some() {
                    TokenRole::Assignment
                } else if needs_command {
                    needs_command = false;
                    TokenRole::CommandWord
                } else {
                    TokenRole::Argument
                }
            }
        };
        roles.push(role);
    }
    roles
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex;

    fn roles(src: &[u8]) -> Vec<TokenRole> {
        classify_tokens(&lex(src), src)
    }

    fn significant(src: &[u8]) -> Vec<(TokenRole, Vec<u8>)> {
        classify_tokens(&lex(src), src)
            .into_iter()
            .zip(lex(src))
            .filter(|(role, _)| *role != TokenRole::Filler)
            .map(|(role, token)| (role, token.text(src).to_vec()))
            .collect()
    }

    #[test]
    fn assignment_then_command_then_arguments() {
        assert_eq!(
            significant(b"FOO=1 bar baz"),
            vec![
                (TokenRole::Assignment, b"FOO=1".to_vec()),
                (TokenRole::CommandWord, b"bar".to_vec()),
                (TokenRole::Argument, b"baz".to_vec()),
            ]
        );
    }

    #[test]
    fn a_separator_starts_a_new_simple_command() {
        assert_eq!(
            significant(b"echo hi; git status"),
            vec![
                (TokenRole::CommandWord, b"echo".to_vec()),
                (TokenRole::Argument, b"hi".to_vec()),
                (TokenRole::Boundary, b";".to_vec()),
                (TokenRole::CommandWord, b"git".to_vec()),
                (TokenRole::Argument, b"status".to_vec()),
            ]
        );
    }

    #[test]
    fn redirects_keep_command_position() {
        assert_eq!(
            significant(b">out cmd arg"),
            vec![
                (TokenRole::RedirectOperator, b">out".to_vec()),
                (TokenRole::CommandWord, b"cmd".to_vec()),
                (TokenRole::Argument, b"arg".to_vec()),
            ]
        );
        assert_eq!(
            significant(b"cmd > out"),
            vec![
                (TokenRole::CommandWord, b"cmd".to_vec()),
                (TokenRole::RedirectOperator, b">".to_vec()),
                (TokenRole::RedirectTarget, b"out".to_vec()),
            ]
        );
    }

    #[test]
    fn an_assignment_is_only_an_assignment_before_the_command_word() {
        assert_eq!(
            significant(b"cmd a=b"),
            vec![
                (TokenRole::CommandWord, b"cmd".to_vec()),
                (TokenRole::Argument, b"a=b".to_vec()),
            ]
        );
    }

    #[test]
    fn roles_line_up_with_the_tokens() {
        let src = b"FOO=1 cmd >out a; b";
        assert_eq!(roles(src).len(), lex(src).len());
    }
}
