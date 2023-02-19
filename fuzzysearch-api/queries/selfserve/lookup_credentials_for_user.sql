SELECT
    credential
FROM
    webauthn_credential
    JOIN account ON account.id = webauthn_credential.user_id
WHERE
    lower(account.email) = lower($1);
