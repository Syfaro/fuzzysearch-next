INSERT INTO api.webauthn_credential
    (id, account_id, credential)
VALUES
    ($2, $1, $3);
