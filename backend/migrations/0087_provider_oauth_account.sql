-- Persist the account selected during provider OAuth setup across restarts.
ALTER TABLE providers ADD COLUMN oauth_account text;
