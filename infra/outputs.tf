output "application_secret_id" {
  value = aws_secretsmanager_secret.app.id
}

output "scanner_repository_url" {
  value = aws_ecr_repository.scanner.repository_url
}

output "data_bucket" {
  value = aws_s3_bucket.data.id
}
