data "aws_iam_policy_document" "lambda_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "news" {
  name_prefix        = "${var.project_name}-news-"
  assume_role_policy = data.aws_iam_policy_document.lambda_assume.json
}

resource "aws_iam_role_policy_attachment" "news_logs" {
  role       = aws_iam_role.news.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_iam_role_policy" "news" {
  role = aws_iam_role.news.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["dynamodb:PutItem"]
        Resource = aws_dynamodb_table.state.arn
      },
      {
        Effect   = "Allow"
        Action   = ["bedrock:InvokeModel"]
        Resource = "*"
      },
      {
        Effect   = "Allow"
        Action   = ["secretsmanager:GetSecretValue"]
        Resource = aws_secretsmanager_secret.app.arn
      },
      {
        Effect = "Allow"
        Action = [
          "sqs:ReceiveMessage",
          "sqs:DeleteMessage",
          "sqs:GetQueueAttributes"
        ]
        Resource = aws_sqs_queue.news.arn
      }
    ]
  })
}

resource "aws_cloudwatch_log_group" "news" {
  name              = "/aws/lambda/${var.project_name}-news-worker"
  retention_in_days = 30
}

resource "aws_lambda_function" "news" {
  function_name    = "${var.project_name}-news-worker"
  filename         = var.news_lambda_zip
  role             = aws_iam_role.news.arn
  handler          = "bootstrap"
  runtime          = "provided.al2023"
  architectures    = [var.lambda_architecture]
  timeout          = 120
  memory_size      = 512
  source_code_hash = filebase64sha256(var.news_lambda_zip)

  environment {
    variables = {
      APP_SECRET_ID        = aws_secretsmanager_secret.app.id
      BEDROCK_MODEL_ID     = var.bedrock_model_id
      DATA_BUCKET          = aws_s3_bucket.data.id
      STATE_TABLE          = aws_dynamodb_table.state.name
      NEWS_QUEUE_URL       = aws_sqs_queue.news.url
      NEWS_REFRESH_SECONDS = "3600"
      RUST_LOG             = "polybot=info"
    }
  }

  depends_on = [
    aws_cloudwatch_log_group.news,
    aws_iam_role_policy_attachment.news_logs,
  ]
}

resource "aws_lambda_event_source_mapping" "news" {
  event_source_arn                   = aws_sqs_queue.news.arn
  function_name                      = aws_lambda_function.news.arn
  batch_size                         = 5
  maximum_batching_window_in_seconds = 5
  function_response_types            = ["ReportBatchItemFailures"]
}
