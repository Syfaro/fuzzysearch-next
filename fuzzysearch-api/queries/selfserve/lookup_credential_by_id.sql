SELECT
    account.id,
    webauthn_credential.credential
FROM
    api.webauthn_credential
    JOIN api.account ON account.id = webauthn_credential.account_id
WHERE
    webauthn_credential.id = $1;
