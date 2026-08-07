resource "aws_ecr_repository" "scanner" {
  name                 = "${var.project_name}-scanner"
  image_tag_mutability = "IMMUTABLE"
  image_scanning_configuration {
    scan_on_push = true
  }
}

resource "aws_ecr_lifecycle_policy" "scanner" {
  repository = aws_ecr_repository.scanner.name
  policy = jsonencode({
    rules = [{
      rulePriority = 1
      description  = "Keep the ten newest scanner images"
      selection = {
        tagStatus   = "any"
        countType   = "imageCountMoreThan"
        countNumber = 10
      }
      action = {
        type = "expire"
      }
    }]
  })
}

resource "aws_ecs_cluster" "main" {
  name = var.project_name
}

resource "aws_cloudwatch_log_group" "scanner" {
  name              = "/ecs/${var.project_name}/scanner"
  retention_in_days = 30
}

resource "aws_iam_role" "ecs_execution" {
  name_prefix        = "${var.project_name}-ecs-execution-"
  assume_role_policy = data.aws_iam_policy_document.ecs_tasks_assume.json
}

data "aws_iam_policy_document" "ecs_tasks_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ecs-tasks.amazonaws.com"]
    }
  }
}

resource "aws_iam_role_policy_attachment" "ecs_execution" {
  role       = aws_iam_role.ecs_execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

resource "aws_iam_role_policy" "ecs_execution_secrets" {
  role = aws_iam_role.ecs_execution.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["secretsmanager:GetSecretValue"]
      Resource = aws_secretsmanager_secret.app.arn
    }]
  })
}

resource "aws_iam_role" "scanner" {
  name_prefix        = "${var.project_name}-scanner-"
  assume_role_policy = data.aws_iam_policy_document.ecs_tasks_assume.json
}

resource "aws_iam_role_policy" "scanner" {
  role = aws_iam_role.scanner.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["s3:PutObject", "s3:GetObject"]
        Resource = "${aws_s3_bucket.data.arn}/*"
      },
      {
        Effect   = "Allow"
        Action   = ["dynamodb:DeleteItem", "dynamodb:GetItem", "dynamodb:PutItem", "dynamodb:UpdateItem"]
        Resource = aws_dynamodb_table.state.arn
      },
      {
        Effect   = "Allow"
        Action   = ["sqs:SendMessage"]
        Resource = aws_sqs_queue.news.arn
      }
    ]
  })
}

locals {
  source_environment = [
    for key, value in var.source_endpoints : {
      name  = "SOURCE_${upper(replace(key, "-", "_"))}_URL"
      value = value
    }
  ]
}

resource "aws_ecs_task_definition" "scanner" {
  family                   = "${var.project_name}-scanner"
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = 1024
  memory                   = 2048
  execution_role_arn       = aws_iam_role.ecs_execution.arn
  task_role_arn            = aws_iam_role.scanner.arn

  container_definitions = jsonencode([{
    name      = "scanner"
    image     = "${aws_ecr_repository.scanner.repository_url}:${var.scanner_image_tag}"
    essential = true
    environment = concat([
      { name = "STORAGE_MODE", value = "aws" },
      { name = "DATA_BUCKET", value = aws_s3_bucket.data.id },
      { name = "STATE_TABLE", value = aws_dynamodb_table.state.name },
      { name = "NEWS_QUEUE_URL", value = aws_sqs_queue.news.url },
      { name = "RUST_LOG", value = "polybot=info" }
    ], local.source_environment)
    logConfiguration = {
      logDriver = "awslogs"
      options = {
        awslogs-group         = aws_cloudwatch_log_group.scanner.name
        awslogs-region        = var.aws_region
        awslogs-stream-prefix = "scanner"
      }
    }
  }])
}

data "aws_iam_policy_document" "scheduler_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["scheduler.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "scheduler" {
  name_prefix        = "${var.project_name}-scheduler-"
  assume_role_policy = data.aws_iam_policy_document.scheduler_assume.json
}

resource "aws_iam_role_policy" "scheduler" {
  role = aws_iam_role.scheduler.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["ecs:RunTask"]
        Resource = aws_ecs_task_definition.scanner.arn
      },
      {
        Effect   = "Allow"
        Action   = ["iam:PassRole"]
        Resource = [aws_iam_role.ecs_execution.arn, aws_iam_role.scanner.arn]
      }
    ]
  })
}

resource "aws_scheduler_schedule" "scanner" {
  name                = "${var.project_name}-scan"
  schedule_expression = "rate(5 minutes)"
  flexible_time_window {
    mode = "OFF"
  }
  target {
    arn      = aws_ecs_cluster.main.arn
    role_arn = aws_iam_role.scheduler.arn
    ecs_parameters {
      task_definition_arn = aws_ecs_task_definition.scanner.arn
      launch_type         = "FARGATE"
      network_configuration {
        assign_public_ip = true
        subnets          = values(aws_subnet.public)[*].id
        security_groups  = [aws_security_group.scanner.id]
      }
    }
  }
}
