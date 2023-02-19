INSERT INTO webauthn_credential
    (credential_id, user_id, credential)
VALUES
    ($2, (SELECT id FROM account WHERE uuid = $1), $3);
