SELECT
    account.uuid
FROM
    webauthn_credential
    JOIN account ON account.id = webauthn_credential.user_id
WHERE
    credential_id = $1;
